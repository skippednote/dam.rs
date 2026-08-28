<?php

declare(strict_types=1);

namespace Drupal\damrs;

/**
 * The transforms damrs will deliver.
 *
 * A transform is a **name**, not a description of an image. damrs resolves one
 * against its built-in profiles and then the tenant's own conversions, and
 * anything else is refused as not deliverable — deliberately, because
 * approximating a typo'd profile would silently hand back a different size than
 * the caller integrated against.
 *
 * So a connector cannot synthesise `w=320,h=320,fit=inside` and expect bytes.
 * It has to name one of these, or a conversion key the tenant has configured.
 * Writing this module against an invented parameter string is exactly the
 * mistake this class exists to stop: it produced a thumbnail URL that damrs
 * would have refused on every request, and nothing in Drupal would have said
 * why.
 *
 * The names are pinned to damrs by `tests/fixtures/transform_names.json`,
 * generated from the Rust — so a rename upstream breaks this module's tests
 * rather than its users' pages.
 */
final class Transforms {

  /**
   * The untransformed original, served from the content-addressed key.
   */
  public const ORIGINAL = 'original';

  /**
   * Square 256px thumbnail, WebP, cropped to fill — what a grid cell wants.
   */
  public const THUMB_256 = 'thumb-256';

  /**
   * Preview at 1024px, fitted inside the box so nothing is cropped out.
   */
  public const PREVIEW_1024 = 'preview-1024';

  /**
   * Web proxy at 2048px, the largest thing that is not the master.
   */
  public const WEB_2048 = 'web-2048';

  /**
   * Every built-in name, for validating configuration.
   *
   * A tenant conversion key is also valid and cannot be listed here, so this is
   * a set to recognise rather than a set to restrict to.
   *
   * @return string[]
   *   The built-in transform names.
   */
  public static function builtIn(): array {
    return [
      self::ORIGINAL,
      self::THUMB_256,
      self::PREVIEW_1024,
      self::WEB_2048,
    ];
  }

}
