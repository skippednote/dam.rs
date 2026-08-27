<?php

declare(strict_types=1);

namespace Drupal\Tests\damrs_media\Kernel;

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
 * The damrs media source against a real entity system.
 *
 * A kernel test rather than a unit test, because the behaviour worth pinning is
 * not what the plugin returns — it is what Drupal *does* with what the plugin
 * returns. `Media::preSave()` assigns the result of `getMetadata()` straight
 * into the mapped field, and that assignment is the whole hazard: the plugin
 * can be perfectly correct in isolation and still cause data loss through how
 * it is called.
 */
#[Group('damrs')]
final class DamrsAssetTest extends KernelTestBase {

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
  ];

  /**
   * The queue of HTTP responses damrs is pretending to give.
   */
  private MockHandler $handler;

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
      ->set('channel', 'web')
      ->set('territory', 'GB')
      ->set('url_ttl', 3600)
      ->save();

    // The transport is replaced rather than the client service, so the code
    // under test goes through the real Client and the real Guzzle stack.
    // Swapping damrs.client for a stub would test the stub.
    $this->handler = new MockHandler();
    $this->container->set('http_client', new GuzzleClient([
      'handler' => HandlerStack::create($this->handler),
    ]));

    $this->createMediaType();
  }

  /**
   * Creates a media type on the damrs source.
   *
   * Three attributes are mapped to real fields, so the write-back that
   * `Media::preSave()` performs is observable.
   */
  private function createMediaType(): void {
    $type = MediaType::create([
      'id' => 'damrs_asset',
      'label' => 'damrs asset',
      'source' => 'damrs_asset',
    ]);
    $type->save();

    $field = $type->getSource()->createSourceField($type);
    $field->getFieldStorageDefinition()->save();
    $field->save();
    $type->set('source_configuration', ['source_field' => $field->getName()]);

    foreach ([['field_t', 'string'], ['field_a', 'string'], ['field_w', 'integer']] as [$name, $type_name]) {
      FieldStorageConfig::create([
        'field_name' => $name,
        'entity_type' => 'media',
        'type' => $type_name,
      ])->save();
      FieldConfig::create([
        'field_name' => $name,
        'entity_type' => 'media',
        'bundle' => 'damrs_asset',
      ])->save();
    }
    $type->set('field_map', [
      'title' => 'field_t',
      'alt_text' => 'field_a',
      'width' => 'field_w',
    ]);
    $type->save();
  }

  /**
   * The source field's machine name, as the plugin created it.
   */
  private function sourceField(): string {
    return MediaType::load('damrs_asset')->getSource()->getConfiguration()['source_field'];
  }

  /**
   * Metadata from damrs lands in the mapped fields.
   */
  public function testMetadataFromDamrsLandsInTheMappedFields(): void {
    $this->handler->append(new Response(200, [], json_encode([
      'filename' => 'harbour.jpg',
      'mime' => 'image/jpeg',
      'bytes' => 482_113,
      'width' => 4000,
      'height' => 3000,
      'version' => 3,
      'metadata' => ['title' => 'The harbour at dawn', 'alt_text' => 'Boats at first light'],
    ])));
    // The thumbnail fetch. Not a real image: nothing here decodes it, and the
    // assertion is about the metadata rather than the bytes.
    $this->handler->append(new Response(200, [], 'thumbnail-bytes'));

    $media = Media::create([
      'bundle' => 'damrs_asset',
      $this->sourceField() => '66666666-7777-8888-9999-aaaaaaaaaaaa',
    ]);
    $media->save();

    $saved = Media::load($media->id());
    self::assertSame('The harbour at dawn', $saved->get('field_t')->value);
    self::assertSame('Boats at first light', $saved->get('field_a')->value);
    self::assertSame('4000', $saved->get('field_w')->value);
  }

  /**
   * An outage must not blank metadata a previous sync cached.
   *
   * This is the property the plugin exists to protect and the one that is
   * invisible from the plugin alone. `Media::preSave()` re-reads metadata when
   * the source field changes and assigns whatever comes back, so a source
   * returning NULL because damrs was unreachable erases the cached title, alt
   * text and dimensions of every item re-saved during the outage. Stale
   * metadata is the correct degraded state; empty metadata is data loss, and it
   * is silent.
   *
   * Confirmed to fail when the fallback in DamrsAsset::getMetadata() is
   * removed: all three fields come back NULL.
   */
  public function testAnOutageDoesNotEraseCachedMetadata(): void {
    $this->handler->append(new Response(200, [], json_encode([
      'filename' => 'harbour.jpg',
      'width' => 4000,
      'version' => 3,
      'metadata' => ['title' => 'The harbour at dawn', 'alt_text' => 'Boats at first light'],
    ])));
    $this->handler->append(new Response(200, [], 'thumbnail-bytes'));

    $media = Media::create([
      'bundle' => 'damrs_asset',
      $this->sourceField() => '66666666-7777-8888-9999-aaaaaaaaaaaa',
    ]);
    $media->save();
    self::assertSame('The harbour at dawn', Media::load($media->id())->get('field_t')->value);

    // Now damrs is gone, and the editor points the item at a different asset —
    // which is exactly what makes Drupal re-read the metadata rather than
    // leaving the fields alone.
    $this->handler->append(new ConnectException(
      'connection refused',
      new Request('GET', 'https://dam.example.test/assets/x'),
    ));
    $this->handler->append(new ConnectException(
      'connection refused',
      new Request('GET', 'https://dam.example.test/d/x'),
    ));

    $media->set($this->sourceField(), '77777777-8888-9999-aaaa-bbbbbbbbbbbb');
    $media->save();

    $after = Media::load($media->id());
    self::assertSame('The harbour at dawn', $after->get('field_t')->value, 'the cached title survives an outage');
    self::assertSame('Boats at first light', $after->get('field_a')->value, 'and the alt text');
    self::assertSame('4000', $after->get('field_w')->value, 'and the dimensions');
  }

  /**
   * A rights refusal on the thumbnail does not stop the save.
   *
   * A library with restricted assets will produce these routinely: the site may
   * reference an asset it is not allowed to render. That has to leave a
   * saveable media item with the generic icon rather than an exception out of
   * the entity save.
   */
  public function testRefusedThumbnailStillSaves(): void {
    $this->handler->append(new Response(200, [], json_encode([
      'filename' => 'restricted.jpg',
      'metadata' => ['title' => 'Restricted'],
    ])));
    $this->handler->append(new Response(403, [], ''));

    $media = Media::create([
      'bundle' => 'damrs_asset',
      $this->sourceField() => '99999999-aaaa-bbbb-cccc-dddddddddddd',
    ]);
    $media->save();

    $saved = Media::load($media->id());
    self::assertSame('Restricted', $saved->get('field_t')->value);
    self::assertFalse($saved->get('thumbnail')->isEmpty(), 'the generic icon stands in');
  }

  /**
   * The asset is fetched once per save, not once per mapped attribute.
   *
   * Three attributes are mapped here and `getMetadata()` is called for each,
   * plus once more for the thumbnail's alt text. Without the per-request memo
   * that is four identical requests every time an editor saves one media item.
   */
  public function testTheAssetIsFetchedOncePerSave(): void {
    $this->handler->append(new Response(200, [], json_encode([
      'filename' => 'once.jpg',
      'width' => 100,
      'metadata' => ['title' => 'Once', 'alt_text' => 'Once'],
    ])));
    $this->handler->append(new Response(200, [], 'thumbnail-bytes'));

    $media = Media::create([
      'bundle' => 'damrs_asset',
      $this->sourceField() => 'aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee',
    ]);
    $media->save();

    // Both queued responses consumed and nothing left over means exactly one
    // asset request and one thumbnail request. A second asset request would
    // have found the queue empty and thrown.
    self::assertCount(0, $this->handler);
    self::assertSame('Once', Media::load($media->id())->get('field_t')->value);
  }

}
