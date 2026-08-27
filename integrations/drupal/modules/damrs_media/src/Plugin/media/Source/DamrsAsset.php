<?php

declare(strict_types=1);

namespace Drupal\damrs_media\Plugin\media\Source;

use Drupal\Core\Entity\EntityFieldManagerInterface;
use Drupal\Core\Entity\EntityTypeManagerInterface;
use Drupal\Core\Field\FieldTypePluginManagerInterface;
use Drupal\Core\File\FileExists;
use Drupal\Core\File\FileSystemInterface;
use Drupal\Core\StringTranslation\TranslatableMarkup;
use Drupal\damrs\Client;
use Drupal\damrs\Signing\SignerFactory;
use Drupal\media\Attribute\MediaSource;
use Drupal\media\MediaInterface;
use Drupal\media\MediaSourceBase;
use GuzzleHttp\ClientInterface as HttpClientInterface;
use Psr\Log\LoggerInterface;
use Symfony\Component\DependencyInjection\ContainerInterface;

/**
 * A media item that *references* a damrs asset.
 *
 * The source field holds an asset id and nothing else. The bytes never enter
 * `sites/default/files`, and that is not a storage optimisation — it is what
 * makes rights authoritative. When a licence expires in the DAM the image stops
 * rendering here, because every rendered URL is resolved by damrs at fetch
 * time. Had Drupal copied the file in, expiry in the DAM would be cosmetic and
 * an expired-licence image would sit on a live site indefinitely.
 *
 * ## Why this class may call the API, and rendering still does not
 *
 * `getMetadata()` looks like a render-path method and is not. Drupal calls it
 * in `Media::preSave()` to copy metadata into the mapped fields and to set the
 * thumbnail; rendering afterwards reads those stored fields and never reaches
 * this plugin. So an API call here happens when an editor saves a media item,
 * which is exactly where waiting on an API is the expected behaviour — and
 * §11.3's rule that painting a page never blocks on damrs stays true.
 *
 * The transform URLs a template renders come from `SignerFactory`, which signs
 * locally and cannot fail on the network at all.
 *
 * ## An outage must not erase what is already cached
 *
 * This is the trap in the way Drupal calls this method. `preSave()` assigns
 * whatever comes back — `$entity->set($field, $source->getMetadata(...))` — so
 * returning NULL because damrs was unreachable would blank the cached title,
 * alt text and dimensions on every media item that happened to be re-saved
 * during the outage. Stale metadata is the correct degraded state; empty
 * metadata is data loss.
 *
 * So when the API cannot answer, this returns **the value already stored in the
 * mapped field** rather than NULL, and the same for the thumbnail. `damrs_sync`
 * is what refreshes metadata when it genuinely changes, driven by damrs's
 * webhooks, which is also why nothing here polls.
 */
#[MediaSource(
  id: "damrs_asset",
  label: new TranslatableMarkup("damrs asset"),
  description: new TranslatableMarkup("An asset held in a damrs library. Drupal stores the reference; the bytes and the rights stay in the DAM."),
  // A plain string: the asset id. Not a file field, and not an entity reference
  // — there is no local entity for it to point at, and that is the whole
  // design.
  allowed_field_types: ["string"],
  default_thumbnail_filename: "no-thumbnail.png",
  thumbnail_alt_metadata_attribute: "alt_text",
)]
final class DamrsAsset extends MediaSourceBase {

  /**
   * Where fetched thumbnails are cached.
   *
   * Local copies of the *thumbnail derivative*, which is the same thing core's
   * oEmbed source does for a remote video: Drupal's Media Library grid renders
   * a local image and image styles need a local stream wrapper, so there is
   * nothing else available. It is a UI cache and not the asset — the master
   * never lands here.
   *
   * Worth being clear about the consequence, because it is the one place the
   * "expiry takes effect immediately" story has an edge: a cached admin
   * thumbnail can outlive the licence. The *rendered* image on the site stops
   * the moment damrs refuses it, because that goes through a signed URL. The
   * thumbnail in the media library is a local file and persists until something
   * clears it, which `damrs_sync` does on a deletion or expiry event.
   */
  private const THUMBNAIL_DIRECTORY = 'public://damrs-thumbnails';

  /**
   * The transform used for the cached thumbnail.
   */
  private const THUMBNAIL_TRANSFORM = 'w=320,h=320,fit=inside,fmt=webp';

  /**
   * Assets already fetched in this request, keyed by id.
   *
   * `getMetadata()` is called once per mapped attribute, so a media type
   * mapping six fields would otherwise make six identical requests per save.
   *
   * @var array<string, array<string, mixed>|null>
   */
  private array $fetched = [];

  public function __construct(
    array $configuration,
    $plugin_id,
    $plugin_definition,
    EntityTypeManagerInterface $entity_type_manager,
    EntityFieldManagerInterface $entity_field_manager,
    FieldTypePluginManagerInterface $field_type_manager,
    $config_factory,
    private readonly Client $client,
    private readonly SignerFactory $signerFactory,
    private readonly HttpClientInterface $httpClient,
    private readonly FileSystemInterface $fileSystem,
    private readonly LoggerInterface $logger,
  ) {
    parent::__construct($configuration, $plugin_id, $plugin_definition, $entity_type_manager, $entity_field_manager, $field_type_manager, $config_factory);
  }

  /**
   * {@inheritdoc}
   */
  public static function create(ContainerInterface $container, array $configuration, $plugin_id, $plugin_definition): static {
    return new static(
      $configuration,
      $plugin_id,
      $plugin_definition,
      $container->get('entity_type.manager'),
      $container->get('entity_field.manager'),
      $container->get('plugin.manager.field.field_type'),
      $container->get('config.factory'),
      $container->get('damrs.client'),
      $container->get('damrs.signer_factory'),
      $container->get('http_client'),
      $container->get('file_system'),
      $container->get('logger.channel.damrs'),
    );
  }

  /**
   * {@inheritdoc}
   */
  public function getMetadataAttributes(): array {
    return [
      'title' => $this->t('Title'),
      'alt_text' => $this->t('Alt text'),
      'description' => $this->t('Description'),
      'mime' => $this->t('MIME type'),
      'bytes' => $this->t('Size in bytes'),
      'width' => $this->t('Width'),
      'height' => $this->t('Height'),
      // Mapped so a site can see which version it is holding, and so
      // `damrs_sync` has something to compare against when a version event
      // arrives.
      'version' => $this->t('Asset version'),
    ];
  }

  /**
   * {@inheritdoc}
   */
  public function getMetadata(MediaInterface $media, $attribute_name) {
    $asset_id = $this->getSourceFieldValue($media);
    if ($asset_id === NULL || $asset_id === '') {
      return parent::getMetadata($media, $attribute_name);
    }

    if ($attribute_name === 'thumbnail_uri') {
      return $this->thumbnailUri($media, $asset_id) ?? $this->existingThumbnail($media) ?? parent::getMetadata($media, 'thumbnail_uri');
    }

    $asset = $this->asset($asset_id);
    if ($asset === NULL) {
      // Unreachable, refused, or gone. Keep what is stored rather than blanking
      // it — see the class docs.
      return $this->stored($media, $attribute_name) ?? parent::getMetadata($media, $attribute_name);
    }

    $value = match ($attribute_name) {
      'title' => $asset['metadata']['title'] ?? $asset['filename'] ?? NULL,
      'alt_text' => $asset['metadata']['alt_text'] ?? NULL,
      'description' => $asset['metadata']['description'] ?? NULL,
      'mime' => $asset['mime'] ?? NULL,
      'bytes' => $asset['bytes'] ?? NULL,
      'width' => $asset['width'] ?? NULL,
      'height' => $asset['height'] ?? NULL,
      'version' => $asset['version'] ?? NULL,
      'default_name' => $asset['filename'] ?? NULL,
      default => NULL,
    };

    // A present asset that simply has no value for this attribute is not a
    // reason to erase a value somebody typed into the mapped field either —
    // Drupal only re-reads metadata when the field is empty or the source
    // changed, and honouring that means not overwriting with nothing.
    return $value ?? $this->stored($media, $attribute_name) ?? parent::getMetadata($media, $attribute_name);
  }

  /**
   * The asset, fetched once per request.
   *
   * @return array|null
   *   The decoded asset, or NULL if damrs could not answer for it.
   */
  private function asset(string $asset_id): ?array {
    if (!array_key_exists($asset_id, $this->fetched)) {
      $this->fetched[$asset_id] = $this->client->asset($asset_id);
    }

    return $this->fetched[$asset_id];
  }

  /**
   * What the mapped field already holds for this attribute, if anything.
   *
   * Reads the media type's own field map, so this follows whatever mapping the
   * site configured rather than assuming field names.
   */
  private function stored(MediaInterface $media, string $attribute_name): mixed {
    $map = $media->bundle->entity?->getFieldMap() ?? [];
    $field = $map[$attribute_name] ?? NULL;
    if ($field === NULL || !$media->hasField($field)) {
      return NULL;
    }
    $items = $media->get($field);
    if ($items->isEmpty()) {
      return NULL;
    }
    $first = $items->first();

    return $first?->{$first->mainPropertyName()};
  }

  /**
   * The thumbnail this media item already has.
   *
   * So that an outage does not replace a good thumbnail with the generic icon.
   */
  private function existingThumbnail(MediaInterface $media): ?string {
    if (!$media->hasField('thumbnail') || $media->get('thumbnail')->isEmpty()) {
      return NULL;
    }
    $file = $media->get('thumbnail')->entity;

    return $file?->getFileUri();
  }

  /**
   * Fetches and caches the thumbnail derivative, returning its local URI.
   *
   * The URL is signed locally, so this needs no API call — only the fetch
   * itself, which is a request to the delivery endpoint like any other
   * client's.
   */
  private function thumbnailUri(MediaInterface $media, string $asset_id): ?string {
    $directory = self::THUMBNAIL_DIRECTORY;
    if (!$this->fileSystem->prepareDirectory($directory, FileSystemInterface::CREATE_DIRECTORY | FileSystemInterface::MODIFY_PERMISSIONS)) {
      $this->logger->error('could not prepare @dir for damrs thumbnails', ['@dir' => $directory]);

      return NULL;
    }

    // Named by asset and version, so a new version is a new file rather than a
    // stale one served from cache. Without the version in the name, a re-crop
    // in the DAM would never appear in the media library.
    $version = (string) ($this->stored($media, 'version') ?? '0');
    $destination = $directory . '/' . preg_replace('/[^a-zA-Z0-9-]/', '', $asset_id) . '-v' . preg_replace('/[^0-9]/', '', $version) . '.webp';
    if (file_exists($destination)) {
      return $destination;
    }

    try {
      $url = $this->signerFactory->deliveryUrl($asset_id, self::THUMBNAIL_TRANSFORM);
    }
    catch (\RuntimeException $e) {
      // No signing secret configured yet. Worth a log rather than a crash: a
      // site mid-configuration should show the generic icon, not a broken media
      // library.
      $this->logger->warning('cannot sign a damrs thumbnail URL: @reason', ['@reason' => $e->getMessage()]);

      return NULL;
    }

    try {
      $response = $this->httpClient->request('GET', $url, [
        'timeout' => 10,
        'http_errors' => FALSE,
      ]);
    }
    catch (\Throwable $e) {
      $this->logger->error('fetching a damrs thumbnail failed: @reason', ['@reason' => $e->getMessage()]);

      return NULL;
    }

    $status = $response->getStatusCode();
    if ($status !== 200) {
      // 403 is a rights refusal and entirely legitimate — an asset this site
      // may not have. Logged at a level that does not shout, because a library
      // with a few restricted assets would otherwise fill the log.
      $this->logger->notice('damrs answered @status for the thumbnail of @asset', [
        '@status' => $status,
        '@asset' => $asset_id,
      ]);

      return NULL;
    }

    $written = $this->fileSystem->saveData((string) $response->getBody(), $destination, FileExists::Replace);

    return $written === FALSE ? NULL : $destination;
  }

}
