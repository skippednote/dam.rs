<?php

declare(strict_types=1);

namespace Drupal\damrs_image_style\Form;

use Drupal\Core\Entity\EntityTypeManagerInterface;
use Drupal\Core\Form\ConfigFormBase;
use Drupal\Core\Form\FormStateInterface;
use Drupal\damrs\Transforms;
use Drupal\damrs_image_style\TransformMap;
use Symfony\Component\DependencyInjection\ContainerInterface;

/**
 * Maps each Drupal image style onto a damrs transform.
 *
 * One row per image style the site has, because the question is per style and
 * an operator should not have to remember which ones exist.
 *
 * The options are the built-in transforms plus free text, and the free text is
 * the point: a tenant can define its own conversions in damrs, and this form
 * cannot know their keys. It validates that a value was given and leaves
 * whether damrs recognises it to damrs — which refuses an unknown transform
 * rather than approximating it, so a typo here fails visibly at render rather
 * than silently delivering the wrong size.
 */
final class MapForm extends ConfigFormBase {

  public function __construct(
    private readonly EntityTypeManagerInterface $entityTypeManager,
    private readonly TransformMap $transformMap,
  ) {}

  /**
   * {@inheritdoc}
   */
  public static function create(ContainerInterface $container): static {
    $form = new static(
      $container->get('entity_type.manager'),
      $container->get('damrs_image_style.transform_map'),
    );
    $form->setConfigFactory($container->get('config.factory'));
    $form->setMessenger($container->get('messenger'));
    $form->setStringTranslation($container->get('string_translation'));

    return $form;
  }

  /**
   * {@inheritdoc}
   */
  public function getFormId(): string {
    return 'damrs_image_style_map';
  }

  /**
   * {@inheritdoc}
   */
  protected function getEditableConfigNames(): array {
    return ['damrs_image_style.settings'];
  }

  /**
   * {@inheritdoc}
   */
  public function buildForm(array $form, FormStateInterface $form_state): array {
    $current = $this->transformMap->all();
    $styles = $this->entityTypeManager->getStorage('image_style')->loadMultiple();

    $form['explanation'] = [
      '#markup' => $this->t('<p>damrs renders a fixed set of named transforms, so a Drupal image style is mapped to one rather than translated into parameters. Built in: @built. A tenant conversion key also works.</p>', [
        '@built' => implode(', ', Transforms::builtIn()),
      ]),
    ];

    $form['fallback'] = [
      '#type' => 'textfield',
      '#title' => $this->t('Fallback transform'),
      '#default_value' => $this->config('damrs_image_style.settings')->get('fallback') ?: Transforms::WEB_2048,
      '#required' => TRUE,
      '#description' => $this->t('Used for an image style with no mapping.'),
    ];

    $form['map'] = [
      '#type' => 'details',
      '#title' => $this->t('Image styles'),
      '#open' => TRUE,
      '#tree' => TRUE,
    ];
    foreach ($styles as $id => $style) {
      $form['map'][$id] = [
        '#type' => 'textfield',
        '#title' => $style->label(),
        '#default_value' => $current[$id] ?? '',
        '#description' => $this->t('Blank falls back.'),
      ];
    }
    if ($styles === []) {
      $form['map']['#description'] = $this->t('This site has no image styles.');
    }

    return parent::buildForm($form, $form_state);
  }

  /**
   * {@inheritdoc}
   */
  public function submitForm(array &$form, FormStateInterface $form_state): void {
    // Blank values are dropped rather than stored as empty strings, so an
    // unmapped style reads as absent and takes the fallback instead of asking
    // damrs for a transform named "".
    $map = array_filter(
      array_map('strval', $form_state->getValue('map') ?: []),
      static fn (string $value): bool => $value !== '',
    );

    $this->config('damrs_image_style.settings')
      ->set('map', $map)
      ->set('fallback', $form_state->getValue('fallback'))
      ->save();

    parent::submitForm($form, $form_state);
  }

}
