<?php

declare(strict_types=1);

namespace Drupal\Tests\damrs_image_style\Kernel;

use Drupal\field\Entity\FieldConfig;
use Drupal\field\Entity\FieldStorageConfig;
use Drupal\KernelTests\KernelTestBase;
use Drupal\media\Entity\Media;
use Drupal\media\Entity\MediaType;
use PHPUnit\Framework\Attributes\Group;

/**
 * Rendering a damrs asset through the formatter.
 *
 * The rendering half of the connector, and the half a template reaches.
 * Two things here are worth pinning beyond "an image appears": that the URL
 * names a transform damrs will resolve, and that the render array does not
 * outlive the URL it carries.
 */
#[Group('damrs')]
final class DamrsImageFormatterTest extends KernelTestBase {

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
    'damrs_image_style',
  ];

  /**
   * The asset id every case renders.
   */
  private const ASSET = '66666666-7777-8888-9999-aaaaaaaaaaaa';

  /**
   * How long a signed URL lasts in these tests.
   */
  private const TTL = 1800;

  /**
   * The media type's source field name.
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
    $this->installConfig(['field', 'system', 'image', 'media', 'damrs_image_style']);

    $this->config('damrs.settings')
      ->set('base_url', 'https://dam.example.test')
      ->set('tenant_id', '11111111-2222-3333-4444-555555555555')
      ->set('signing_key_id', 'k1')
      ->set('signing_secret', 'test-secret')
      ->set('channel', 'web')
      ->set('territory', 'GB')
      ->set('url_ttl', self::TTL)
      ->save();

    $type = MediaType::create([
      'id' => 'damrs_asset',
      'label' => 'damrs asset',
      'source' => 'damrs_asset',
    ]);
    $type->save();
    $field = $type->getSource()->createSourceField($type);
    $field->getFieldStorageDefinition()->save();
    $field->save();
    $type->set('source_configuration', ['source_field' => $field->getName()])->save();
    $this->sourceField = $field->getName();

    FieldStorageConfig::create([
      'field_name' => 'field_alt',
      'entity_type' => 'media',
      'type' => 'string',
    ])->save();
    FieldConfig::create([
      'field_name' => 'field_alt',
      'entity_type' => 'media',
      'bundle' => 'damrs_asset',
    ])->save();
  }

  /**
   * Builds the formatter's elements for one asset.
   *
   * @param array $settings
   *   Formatter settings to apply.
   *
   * @return array
   *   The render array the formatter produced.
   */
  private function renderAsset(array $settings = []): array {
    $media = Media::create([
      'bundle' => 'damrs_asset',
      $this->sourceField => self::ASSET,
      'field_alt' => 'A boat at dawn',
    ]);

    $formatter = \Drupal::service('plugin.manager.field.formatter')->createInstance(
      'damrs_asset_image',
      [
        'field_definition' => $media->get($this->sourceField)->getFieldDefinition(),
        'settings' => $settings + ['image_style' => '', 'alt_field' => ''],
        'label' => 'hidden',
        'view_mode' => 'default',
        'third_party_settings' => [],
      ],
    );

    return $formatter->viewElements($media->get($this->sourceField), 'en');
  }

  /**
   * The URL names the transform the image style maps to.
   */
  public function testTheUrlNamesTheMappedTransform(): void {
    $elements = $this->renderAsset(['image_style' => 'thumbnail']);

    self::assertArrayHasKey(0, $elements);
    self::assertSame('image', $elements[0]['#theme']);
    // The transform is inside the signed payload rather than a query parameter,
    // so this decodes it rather than pattern-matching the URL.
    self::assertSame('thumb-256', $this->transformOf($elements[0]['#uri']));
  }

  /**
   * An image style with no mapping falls back rather than asking for "".
   */
  public function testAnUnmappedStyleFallsBack(): void {
    $elements = $this->renderAsset(['image_style' => 'a_style_nobody_mapped']);

    self::assertSame('web-2048', $this->transformOf($elements[0]['#uri']));
  }

  /**
   * The render must not outlive the URL it carries.
   *
   * The trap this formatter exists to avoid. A render array cached for longer
   * than the signed URL's TTL is, after that TTL, a cached page full of images
   * damrs refuses — and nothing in the logs connects the two.
   */
  public function testTheRenderDoesNotOutliveTheUrl(): void {
    $elements = $this->renderAsset(['image_style' => 'thumbnail']);

    self::assertSame(self::TTL, $elements[0]['#cache']['max-age']);
    self::assertContains('config:damrs.settings', $elements[0]['#cache']['tags']);
    self::assertContains(
      'config:damrs_image_style.settings',
      $elements[0]['#cache']['tags'],
      'the mapping changes what this renders, so it has to invalidate it',
    );
  }

  /**
   * Alt text comes from the configured field, and is empty when unset.
   */
  public function testAltTextComesFromTheConfiguredField(): void {
    self::assertSame('A boat at dawn', $this->renderAsset(['alt_field' => 'field_alt'])[0]['#alt']);
    self::assertSame('', $this->renderAsset()[0]['#alt']);
  }

  /**
   * With no signing secret there is no image, rather than a broken one.
   */
  public function testNoSigningSecretRendersNothing(): void {
    $this->config('damrs.settings')->set('signing_secret', '')->save();

    self::assertSame([], $this->renderAsset(['image_style' => 'thumbnail']));
  }

  /**
   * The transform inside a delivery token.
   *
   * @param string $url
   *   A delivery URL.
   *
   * @return string
   *   The transform the token names.
   */
  private function transformOf(string $url): string {
    $token = substr($url, (int) strrpos($url, '/') + 1);
    [$payload] = explode('.', $token, 2);
    $bytes = base64_decode(strtr($payload, '-_', '+/'), TRUE);
    self::assertNotFalse($bytes, 'the payload must be base64url');

    // Version byte, then length-prefixed fields: purpose, tenant, asset,
    // transform. Walked rather than guessed at an offset, because the uuid
    // fields are fixed width only by convention of what they hold.
    $offset = 1;
    $fields = [];
    for ($i = 0; $i < 4; $i++) {
      $length = unpack('N', substr($bytes, $offset, 4))[1];
      $offset += 4;
      $fields[] = substr($bytes, $offset, $length);
      $offset += $length;
    }

    return $fields[3];
  }

}
