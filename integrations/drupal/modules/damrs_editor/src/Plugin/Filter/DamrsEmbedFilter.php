<?php

declare(strict_types=1);

namespace Drupal\damrs_editor\Plugin\Filter;

use Drupal\Component\Utility\Html;
use Drupal\Core\Config\ConfigFactoryInterface;
use Drupal\Core\Form\FormStateInterface;
use Drupal\Core\Plugin\ContainerFactoryPluginInterface;
use Drupal\Core\StringTranslation\TranslatableMarkup;
use Drupal\damrs\Client;
use Drupal\filter\Attribute\Filter;
use Drupal\filter\FilterProcessResult;
use Drupal\filter\Plugin\FilterBase;
use Drupal\filter\Plugin\FilterInterface;
use Symfony\Component\DependencyInjection\ContainerInterface;

/**
 * Turns a pasted damrs asset URL into an inline image.
 *
 * ## What core cannot do here, and why this exists.
 *
 * Inserting a damrs asset as a *media entity* already works with no code from
 * this module: `damrs_media` makes an ordinary media type, so core's Media
 * Library button and the `media_embed` filter handle it. Verified rather than
 * assumed — a `<drupal-media>` tag pointing at a damrs media item renders our
 * signed URL today.
 *
 * What core cannot do is resolve a *pasted URL*. Its OEmbed source discovers
 * providers through the public registry and fetches without a credential, and
 * damrs's oEmbed provider is authenticated on purpose: an unauthenticated
 * endpoint that turns an asset id into a filename, a size and a preview is an
 * enumeration API for somebody's whole library. So the fetch happens here,
 * server-side, with the connector's key — which is precisely the arrangement
 * damrs's own oEmbed module says it expects.
 *
 * ## The cache lifetime again.
 *
 * The `url` in an oEmbed response is a signed delivery token and expires, and
 * damrs reports a `cache_age` deliberately shorter than that token's own
 * lifetime. This filter's result is cached separately from the formatter's
 * render array, so the same trap is here in a second form: a filtered body
 * cached for longer than `cache_age` serves an expired URL. The result carries
 * that max-age rather than assuming anything about it.
 *
 * ## Links only, never bare text.
 *
 * A URL is rewritten when it is the whole of an `<a href>`, and not when it
 * merely appears in prose. Rewriting text would mean an author who wanted to
 * *mention* an asset URL — in documentation about the DAM, say — could not,
 * and a filter that cannot be escaped is one people work around.
 */
#[Filter(
  id: 'damrs_embed',
  title: new TranslatableMarkup('Embed damrs assets from pasted links'),
  type: FilterInterface::TYPE_TRANSFORM_REVERSIBLE,
  settings: ['max_width' => 1024],
)]
final class DamrsEmbedFilter extends FilterBase implements ContainerFactoryPluginInterface {

  public function __construct(
    array $configuration,
    $plugin_id,
    $plugin_definition,
    private readonly Client $client,
    private readonly ConfigFactoryInterface $configFactory,
  ) {
    parent::__construct($configuration, $plugin_id, $plugin_definition);
  }

  /**
   * {@inheritdoc}
   */
  public static function create(ContainerInterface $container, array $configuration, $plugin_id, $plugin_definition): static {
    return new static(
      $configuration,
      $plugin_id,
      $plugin_definition,
      $container->get('damrs.client'),
      $container->get('config.factory'),
    );
  }

  /**
   * {@inheritdoc}
   */
  public function settingsForm(array $form, FormStateInterface $form_state): array {
    $form['max_width'] = [
      '#type' => 'number',
      '#title' => $this->t('Maximum width to request'),
      '#default_value' => $this->settings['max_width'] ?? 1024,
      '#min' => 16,
      '#description' => $this->t('A hint to damrs, so it picks a rendition rather than serving the largest one into a narrow column.'),
    ];

    return $form;
  }

  /**
   * {@inheritdoc}
   */
  public function process($text, $langcode): FilterProcessResult {
    $result = new FilterProcessResult($text);

    $base = rtrim((string) $this->configFactory->get('damrs.settings')->get('base_url'), '/');
    if ($base === '' || !str_contains($text, $base . '/assets/')) {
      // Nothing of ours in this body. Returning early matters: `Html::load`
      // on every filtered field on a site is not free, and most bodies have no
      // damrs link in them at all.
      return $result;
    }

    $dom = Html::load($text);
    $xpath = new \DOMXPath($dom);
    $links = $xpath->query('//a[@href]');
    if ($links === FALSE) {
      return $result;
    }

    // The tightest cache age any embed on this body reported. One expired URL
    // is enough to break the page, so the shortest wins.
    $max_age = NULL;
    $changed = FALSE;

    foreach ($links as $link) {
      if (!$link instanceof \DOMElement) {
        continue;
      }
      $href = $link->getAttribute('href');
      if (!str_starts_with($href, $base . '/assets/')) {
        continue;
      }

      $oembed = $this->client->oembed($href, (int) ($this->settings['max_width'] ?? 1024));
      if ($oembed === NULL) {
        // Not describable: damrs is unreachable, or this is an asset the site
        // may not see. The link is left exactly as the author wrote it, which
        // still works for anyone who does have access — and is what a filter
        // should do when it cannot improve on its input.
        continue;
      }

      $replacement = $this->element($dom, $oembed, $link->textContent);
      if ($replacement === NULL) {
        continue;
      }
      $link->parentNode?->replaceChild($replacement, $link);
      $changed = TRUE;

      $age = isset($oembed['cache_age']) ? (int) $oembed['cache_age'] : NULL;
      if ($age !== NULL && ($max_age === NULL || $age < $max_age)) {
        $max_age = $age;
      }
    }

    if (!$changed) {
      return $result;
    }

    $result->setProcessedText(Html::serialize($dom));
    if ($max_age !== NULL) {
      // Not `min()` against an existing value: a FilterProcessResult starts at
      // permanent, so this is the first thing to bound it.
      $result->setCacheMaxAge($max_age);
    }
    $result->addCacheTags(['config:damrs.settings']);

    return $result;
  }

  /**
   * The element an oEmbed response should become.
   *
   * A `photo` becomes an image. Anything else — damrs answers `link` for a
   * video, a PDF, an audio file — becomes a link with its thumbnail, because
   * claiming to embed something this has no player for would render a broken
   * box where the author expected a card.
   */
  private function element(\DOMDocument $dom, array $oembed, string $label): ?\DOMNode {
    $type = (string) ($oembed['type'] ?? '');
    $title = (string) ($oembed['title'] ?? $label);

    if ($type === 'photo' && !empty($oembed['url'])) {
      $img = $dom->createElement('img');
      $img->setAttribute('src', (string) $oembed['url']);
      $img->setAttribute('alt', $title);
      $img->setAttribute('loading', 'lazy');
      foreach (['width', 'height'] as $dimension) {
        if (!empty($oembed[$dimension])) {
          $img->setAttribute($dimension, (string) (int) $oembed[$dimension]);
        }
      }

      return $img;
    }

    if (!empty($oembed['thumbnail_url'])) {
      $anchor = $dom->createElement('a');
      $anchor->setAttribute('href', (string) ($oembed['url'] ?? ''));
      $thumb = $dom->createElement('img');
      $thumb->setAttribute('src', (string) $oembed['thumbnail_url']);
      $thumb->setAttribute('alt', $title);
      $thumb->setAttribute('loading', 'lazy');
      $anchor->appendChild($thumb);

      return $anchor;
    }

    // Described, but with nothing to show. Leaving the author's link alone is
    // better than replacing it with an empty element.
    return NULL;
  }

  /**
   * {@inheritdoc}
   */
  public function tips($long = FALSE): string {
    return (string) $this->t('Paste a damrs asset link on its own and it will be embedded.');
  }

}
