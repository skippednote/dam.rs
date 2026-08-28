<?php

declare(strict_types=1);

namespace Drupal\Tests\damrs\Unit;

use Drupal\damrs\Transforms;
use PHPUnit\Framework\Attributes\CoversClass;
use PHPUnit\Framework\Attributes\Group;
use PHPUnit\Framework\TestCase;

/**
 * Holds the transform names to the ones damrs actually serves.
 *
 * The fixture comes from `cargo run -p dam-media --example transform_names`, so
 * this compares against the registry that will refuse the request rather than
 * against this module's idea of it. A renamed or removed profile upstream fails
 * here instead of turning every image on a site into a refusal.
 */
#[Group('damrs')]
#[CoversClass(Transforms::class)]
final class TransformsTest extends TestCase {

  /**
   * The fixture, decoded.
   *
   * @return array
   *   The decoded transform_names.json.
   */
  private static function fixture(): array {
    $path = __DIR__ . '/../../fixtures/transform_names.json';
    $raw = file_get_contents($path);
    if ($raw === FALSE) {
      throw new \RuntimeException("missing $path; regenerate it with "
        . 'cargo run -p dam-media --example transform_names');
    }

    return json_decode($raw, TRUE, 512, JSON_THROW_ON_ERROR);
  }

  /**
   * Every name this module can emit is one damrs will resolve.
   */
  public function testTheBuiltInNamesMatchDamrs(): void {
    $data = self::fixture();
    $expected = array_merge(
      [$data['original']],
      array_column($data['profiles'], 'name'),
    );

    sort($expected);
    $actual = Transforms::builtIn();
    sort($actual);

    self::assertSame($expected, $actual);
  }

  /**
   * The thumbnail constant is the profile whose role is the thumbnail.
   *
   * Named rather than assumed: picking `preview-1024` for a media library grid
   * would work and waste bandwidth on every cell, which is the kind of thing
   * nobody notices.
   */
  public function testTheThumbnailConstantIsTheThumbnailProfile(): void {
    $thumbnails = array_values(array_filter(
      self::fixture()['profiles'],
      static fn (array $p): bool => $p['role'] === 'thumbnail',
    ));

    self::assertCount(1, $thumbnails, 'damrs has exactly one thumbnail profile');
    self::assertSame($thumbnails[0]['name'], Transforms::THUMB_256);
  }

}
