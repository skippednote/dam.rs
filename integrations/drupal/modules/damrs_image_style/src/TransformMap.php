<?php

declare(strict_types=1);

namespace Drupal\damrs_image_style;

use Drupal\Core\Config\ConfigFactoryInterface;
use Drupal\damrs\Transforms;

/**
 * Which damrs transform stands in for which Drupal image style.
 *
 * ## Why this is a mapping and not a translation.
 *
 * The obvious design is to read a Drupal image style's effects — scale to 800,
 * crop to a ratio, convert to WebP — and emit the equivalent damrs transform.
 * It cannot work, and the reason is on damrs's side rather than ours: a
 * transform is a *name*, resolved against the built-in profiles and the
 * tenant's conversions, and anything else is refused as not deliverable. That
 * refusal is deliberate. Approximating an unrecognised transform would hand
 * back a different size than the caller integrated against, silently.
 *
 * So a synthesised `w=800,fit=cover` is not a transform damrs will serve, and
 * no amount of reading effects changes that. What a site can do is say which of
 * the transforms damrs *does* render each of its image styles corresponds to,
 * which is a decision about intent rather than arithmetic — `thumbnail` means
 * the grid cell, whatever pixel dimensions the local style happened to use.
 *
 * A site wanting an exact size damrs does not offer adds a conversion in damrs
 * and maps to its key. That keeps one place deciding what renditions exist,
 * which is what makes the derivative cache and the rights model coherent.
 */
final class TransformMap {

  public function __construct(
    private readonly ConfigFactoryInterface $configFactory,
  ) {}

  /**
   * The transform for an image style, or the fallback.
   *
   * @param string|null $style
   *   A Drupal image style id, or NULL to ask for the fallback directly.
   *
   * @return string
   *   A damrs transform name.
   */
  public function forStyle(?string $style): string {
    $config = $this->configFactory->get('damrs_image_style.settings');
    $fallback = (string) ($config->get('fallback') ?: Transforms::WEB_2048);

    if ($style === NULL || $style === '') {
      return $fallback;
    }

    $map = $config->get('map') ?: [];

    return (string) ($map[$style] ?? $fallback);
  }

  /**
   * The current mapping, for the settings form.
   *
   * @return array
   *   Damrs transform names, keyed by Drupal image style id.
   */
  public function all(): array {
    return $this->configFactory->get('damrs_image_style.settings')->get('map') ?: [];
  }

}
