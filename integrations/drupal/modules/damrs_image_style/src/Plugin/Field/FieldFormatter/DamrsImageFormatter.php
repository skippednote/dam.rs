<?php

declare(strict_types=1);

namespace Drupal\damrs_image_style\Plugin\Field\FieldFormatter;

use Drupal\Core\Config\ConfigFactoryInterface;
use Drupal\Core\Field\Attribute\FieldFormatter;
use Drupal\Core\Field\FieldItemListInterface;
use Drupal\Core\Field\FormatterBase;
use Drupal\Core\Form\FormStateInterface;
use Drupal\Core\Plugin\ContainerFactoryPluginInterface;
use Drupal\Core\StringTranslation\TranslatableMarkup;
use Drupal\damrs\Signing\SignerFactory;
use Drupal\damrs\Transforms;
use Drupal\damrs_image_style\TransformMap;
use Symfony\Component\DependencyInjection\ContainerInterface;

/**
 * Renders a damrs asset id as an image, from a locally signed URL.
 *
 * This is where the connector finally puts a picture on a page, and everything
 * it does happens without touching the network. The URL is signed here from the
 * shared secret, so painting a page cannot block on damrs and an outage
 * upstream leaves a stale-but-working page rather than a white screen.
 *
 * ## The cache lifetime is the URL lifetime, and this is the subtle part
 *
 * A signed URL expires. A render array cached for longer than the URL's TTL
 * will, after that TTL, be served from cache carrying a URL damrs now refuses —
 * so the page renders and every image on it is broken, with nothing in the logs
 * connecting the two.
 *
 * So the formatter caps its own max-age at the configured TTL. That does bound
 * how long a page holding damrs images can sit in the page cache, which is a
 * real cost and the right one: the alternative is an operator being told to
 * keep two unrelated numbers in the correct order by hand, and finding out they
 * did not when a customer reports broken images.
 */
#[FieldFormatter(
  id: 'damrs_asset_image',
  label: new TranslatableMarkup('damrs image'),
  field_types: ['string'],
)]
final class DamrsImageFormatter extends FormatterBase implements ContainerFactoryPluginInterface {

  public function __construct(
    $plugin_id,
    $plugin_definition,
    $field_definition,
    array $settings,
    $label,
    $view_mode,
    array $third_party_settings,
    private readonly SignerFactory $signerFactory,
    private readonly TransformMap $transformMap,
    private readonly ConfigFactoryInterface $configFactory,
  ) {
    parent::__construct($plugin_id, $plugin_definition, $field_definition, $settings, $label, $view_mode, $third_party_settings);
  }

  /**
   * {@inheritdoc}
   */
  public static function create(ContainerInterface $container, array $configuration, $plugin_id, $plugin_definition): static {
    return new static(
      $plugin_id,
      $plugin_definition,
      $configuration['field_definition'],
      $configuration['settings'],
      $configuration['label'],
      $configuration['view_mode'],
      $configuration['third_party_settings'],
      $container->get('damrs.signer_factory'),
      $container->get('damrs_image_style.transform_map'),
      $container->get('config.factory'),
    );
  }

  /**
   * {@inheritdoc}
   */
  public static function defaultSettings(): array {
    return [
      // An image style rather than a transform, so a site configures this the
      // way it configures every other image field and the mapping stays in one
      // place. Empty means the mapping's fallback.
      'image_style' => '',
      'alt_field' => '',
    ] + parent::defaultSettings();
  }

  /**
   * {@inheritdoc}
   */
  public function settingsForm(array $form, FormStateInterface $form_state): array {
    $form['image_style'] = [
      '#type' => 'textfield',
      '#title' => $this->t('Image style'),
      '#default_value' => $this->getSetting('image_style'),
      '#description' => $this->t('The Drupal image style whose damrs transform to render. Blank uses the mapping fallback. Map styles to transforms at /admin/config/media/damrs/image-styles.'),
    ];
    $form['alt_field'] = [
      '#type' => 'textfield',
      '#title' => $this->t('Alt text field'),
      '#default_value' => $this->getSetting('alt_field'),
      '#description' => $this->t('Field on this entity holding the alt text. Blank renders an empty alt, which is correct for a decorative image and wrong for any other — so set it.'),
    ];

    return $form;
  }

  /**
   * {@inheritdoc}
   */
  public function settingsSummary(): array {
    $style = $this->getSetting('image_style');

    return [
      $this->t('Transform: @transform', [
        '@transform' => $this->transformMap->forStyle($style ?: NULL),
      ]),
    ];
  }

  /**
   * {@inheritdoc}
   */
  public function viewElements(FieldItemListInterface $items, $langcode): array {
    $transform = $this->transformMap->forStyle($this->getSetting('image_style') ?: NULL);
    $ttl = (int) ($this->configFactory->get('damrs.settings')->get('url_ttl') ?: 3600);
    $alt_field = (string) $this->getSetting('alt_field');
    $entity = $items->getEntity();

    $elements = [];
    foreach ($items as $delta => $item) {
      $asset_id = $item->value;
      if ($asset_id === NULL || $asset_id === '') {
        continue;
      }

      try {
        $url = $this->signerFactory->deliveryUrl((string) $asset_id, $transform);
      }
      catch (\RuntimeException $e) {
        // Unconfigured signing secret. Rendering nothing is right: an <img>
        // pointing at an unsigned URL would be a broken image on a live page,
        // and this way the field is simply absent until somebody finishes
        // configuring the module.
        continue;
      }

      $alt = '';
      if ($alt_field !== '' && $entity->hasField($alt_field) && !$entity->get($alt_field)->isEmpty()) {
        $alt = (string) $entity->get($alt_field)->value;
      }

      $elements[$delta] = [
        '#theme' => 'image',
        '#uri' => $url,
        '#alt' => $alt,
        '#attributes' => ['loading' => 'lazy'],
        '#cache' => [
          // See the class docs: a render cached past the URL's life serves a
          // URL damrs refuses.
          'max-age' => $ttl,
          // The mapping and the connection settings both change what this
          // renders, so both have to invalidate it.
          'tags' => [
            'config:damrs.settings',
            'config:damrs_image_style.settings',
          ],
        ],
      ];
    }

    return $elements;
  }

  /**
   * {@inheritdoc}
   */
  public static function isApplicable($field_definition): bool {
    // Only the source field of a damrs media type, not every string field on
    // the site. Offering this formatter for an arbitrary text field would put a
    // broken image where somebody expected their text.
    $entity_type = $field_definition->getTargetEntityTypeId();
    if ($entity_type !== 'media') {
      return FALSE;
    }
    $bundle = $field_definition->getTargetBundle();
    if ($bundle === NULL) {
      return FALSE;
    }
    $type = \Drupal::entityTypeManager()->getStorage('media_type')->load($bundle);

    return $type !== NULL && $type->getSource()->getPluginId() === 'damrs_asset';
  }

  /**
   * The transforms a site may map to, for the settings form's benefit.
   *
   * @return string[]
   *   The built-in transform names.
   */
  public static function knownTransforms(): array {
    return Transforms::builtIn();
  }

}
