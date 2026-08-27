<?php

declare(strict_types=1);

namespace Drupal\Tests\damrs_sync\Kernel;

use Drupal\damrs_sync\UnreachableException;
use Drupal\field\Entity\FieldConfig;
use Drupal\field\Entity\FieldStorageConfig;
use Drupal\KernelTests\KernelTestBase;
use Drupal\media\Entity\Media;
use Drupal\media\Entity\MediaType;
use GuzzleHttp\Client as GuzzleClient;
use GuzzleHttp\Exception\ConnectException;
use GuzzleHttp\Handler\MockHandler;
use GuzzleHttp\HandlerStack;
use GuzzleHttp\Psr7\Request;
use GuzzleHttp\Psr7\Response;
use PHPUnit\Framework\Attributes\Group;

/**
 * Applying damrs events to the media items that reference an asset.
 *
 * The case worth the whole file is the outage one. `damrs_media` falls back to
 * the value already in a mapped field so that an outage cannot blank cached
 * metadata, and refreshing works by *clearing* those fields so Drupal re-reads
 * them. Together, the first version of these two modules erased the metadata a
 * refresh was supposed to update — each correct alone, destructive in
 * combination, and both suites green.
 */
#[Group('damrs')]
final class EventApplierTest extends KernelTestBase {

  /**
   * {@inheritdoc}
   */
  protected static $modules = [
    'system',
    'user',
    'field',
    'file',
    'image',
    'media',
    'damrs',
    'damrs_media',
    'damrs_sync',
  ];

  private const ASSET = '66666666-7777-8888-9999-aaaaaaaaaaaa';

  /**
   * Queued HTTP responses damrs is pretending to give.
   */
  private MockHandler $handler;

  /**
   * The media type's source field.
   */
  private string $sourceField;

  /**
   * {@inheritdoc}
   */
  protected function setUp(): void {
    parent::setUp();
    $this->installEntitySchema('user');
    $this->installEntitySchema('file');
    $this->installEntitySchema('media');
    $this->installSchema('file', ['file_usage']);
    $this->installConfig(['field', 'system', 'image', 'media']);

    $this->config('damrs.settings')
      ->set('base_url', 'https://dam.example.test')
      ->set('tenant_id', '11111111-2222-3333-4444-555555555555')
      ->set('api_key', 'test-key')
      ->set('signing_key_id', 'k1')
      ->set('signing_secret', 'test-secret')
      ->set('url_ttl', 1800)
      ->save();

    $this->handler = new MockHandler();
    $this->container->set('http_client', new GuzzleClient([
      'handler' => HandlerStack::create($this->handler),
    ]));

    $type = MediaType::create([
      'id' => 'damrs_asset',
      'label' => 'damrs asset',
      'source' => 'damrs_asset',
    ]);
    $type->save();
    $field = $type->getSource()->createSourceField($type);
    $field->getFieldStorageDefinition()->save();
    $field->save();
    $this->sourceField = $field->getName();
    $type->set('source_configuration', ['source_field' => $this->sourceField]);

    FieldStorageConfig::create([
      'field_name' => 'field_t',
      'entity_type' => 'media',
      'type' => 'string',
    ])->save();
    FieldConfig::create([
      'field_name' => 'field_t',
      'entity_type' => 'media',
      'bundle' => 'damrs_asset',
    ])->save();
    $type->set('field_map', ['title' => 'field_t'])->save();
  }

  /**
   * A media item referencing the asset, with a cached title.
   */
  private function item(string $title = 'cached title'): Media {
    $media = Media::create([
      'bundle' => 'damrs_asset',
      $this->sourceField => self::ASSET,
      'field_t' => $title,
      'name' => 'a media item',
    ]);

    // The save reaches the media source, which asks damrs for the thumbnail's
    // alt text and for the thumbnail bytes even though the mapped title is
    // already set. Those requests are answered with failures here: the source
    // falls back, the cached title is untouched, and the fixture stays about
    // what the applier does rather than about how an item came to exist.
    for ($i = 0; $i < 4; $i++) {
      $this->handler->append(new ConnectException(
        'not what this test is about',
        new Request('GET', 'https://dam.example.test/setup'),
      ));
    }
    $media->save();
    // Whatever was not consumed is cleared, so every test starts from an empty
    // queue and `assertCount(0, ...)` means what it says.
    $this->handler->reset();

    return $media;
  }

  /**
   * The applier under test.
   */
  private function applier(): object {
    return $this->container->get('damrs_sync.applier');
  }

  /**
   * The stored title, freshly loaded.
   */
  private function titleOf(Media $media): ?string {
    return Media::load($media->id())->get('field_t')->value;
  }

  /**
   * A refresh with damrs answering replaces the cached metadata.
   */
  public function testRefreshAppliesTheNewMetadata(): void {
    $media = $this->item('the old title');

    // Once for the applier's reachability check, once for the source's own read
    // during the save that follows.
    $body = json_encode(['filename' => 'x.jpg', 'metadata' => ['title' => 'the new title']]);
    $this->handler->append(new Response(200, [], $body));
    $this->handler->append(new Response(200, [], $body));
    $this->handler->append(new Response(200, [], 'thumb'));

    $touched = $this->applier()->apply('asset.metadata_updated', self::ASSET, FALSE);

    self::assertSame(1, $touched);
    self::assertSame('the new title', $this->titleOf($media));
  }

  /**
   * An unreachable damrs must not erase what is cached.
   *
   * The regression this module was written with. Clearing the mapped fields to
   * force a re-read removes the value `damrs_media`'s fallback would have
   * returned, so a refresh during an outage blanked the metadata instead of
   * leaving it alone.
   */
  public function testUnreachableDamrsLeavesTheMetadataAlone(): void {
    $media = $this->item('cached title that must survive');

    $this->handler->append(new ConnectException(
      'connection refused',
      new Request('GET', 'https://dam.example.test/assets/x'),
    ));

    try {
      $this->applier()->apply('asset.metadata_updated', self::ASSET, FALSE);
      self::fail('an unreachable damrs must not be treated as a successful refresh');
    }
    catch (UnreachableException $e) {
      // The queue worker turns this into a suspend, so the item is retried.
      self::assertStringContainsString(self::ASSET, $e->getMessage());
    }

    self::assertSame('cached title that must survive', $this->titleOf($media));
  }

  /**
   * An asset damrs says is gone keeps its last known metadata.
   *
   * Not an exception: retrying will not make a deleted asset reappear, so an
   * item that threw here would never drain. And not a blanking either — an
   * editor looking at a media item whose asset has gone is better served by the
   * last title it had than by an empty row.
   */
  public function testAssetThatIsGoneKeepsItsMetadata(): void {
    $media = $this->item('last known title');

    $this->handler->append(new Response(404, [], ''));

    $touched = $this->applier()->apply('asset.metadata_updated', self::ASSET, FALSE);

    self::assertSame(0, $touched);
    self::assertSame('last known title', $this->titleOf($media));
    self::assertCount(0, $this->handler, 'exactly one request, and no refresh after it');
  }

  /**
   * A deletion asks damrs nothing.
   *
   * The asset is gone by definition, so a refresh would answer 404 — and if
   * that refresh required an answer, the item would be queued forever.
   */
  public function testDeletionAsksDamrsNothing(): void {
    $media = $this->item('title at deletion');

    $touched = $this->applier()->apply('asset.deleted', self::ASSET, FALSE);

    self::assertSame(0, $touched);
    self::assertSame('title at deletion', $this->titleOf($media));
    self::assertTrue(Media::load($media->id())->isPublished(), 'off by default');
  }

  /**
   * With the setting on, a deletion unpublishes.
   */
  public function testDeletionCanUnpublishWhenAsked(): void {
    $media = $this->item();

    $touched = $this->applier()->apply('asset.deleted', self::ASSET, TRUE);

    self::assertSame(1, $touched);
    self::assertFalse(Media::load($media->id())->isPublished());
  }

  /**
   * Unpublishing upstream unpublishes here, and is idempotent.
   */
  public function testUnpublishingIsIdempotent(): void {
    $media = $this->item();

    self::assertSame(1, $this->applier()->apply('asset.unpublished', self::ASSET, FALSE));
    self::assertFalse(Media::load($media->id())->isPublished());
    // A second delivery of the same event changes nothing and reports so, since
    // damrs retries and a no-op save would bump `changed` and invalidate caches
    // for nothing.
    self::assertSame(0, $this->applier()->apply('asset.unpublished', self::ASSET, FALSE));
  }

  /**
   * An event for an asset nobody references is a no-op.
   *
   * Ordinary rather than exceptional: a library is bigger than any one site's
   * use of it, so most events are about assets this site has never referenced.
   */
  public function testEventForUnreferencedAssetDoesNothing(): void {
    $this->item();

    self::assertSame(
      0,
      $this->applier()->apply('asset.metadata_updated', '99999999-8888-7777-6666-555555555555', FALSE),
    );
    self::assertCount(0, $this->handler, 'and it does not ask damrs about it either');
  }

  /**
   * An unknown event is ignored rather than retried forever.
   */
  public function testUnknownEventIsIgnored(): void {
    $this->item();

    self::assertSame(0, $this->applier()->apply('asset.something_new', self::ASSET, FALSE));
  }

}
