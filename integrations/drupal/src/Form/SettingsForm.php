<?php

declare(strict_types=1);

namespace Drupal\damrs\Form;

use Drupal\Core\Form\ConfigFormBase;
use Drupal\Core\Form\FormStateInterface;
use Drupal\damrs\Client;
use Symfony\Component\DependencyInjection\ContainerInterface;

/**
 * Where a site is pointed at a damrs library.
 *
 * The connection check is on submit rather than behind a separate button,
 * because the failure this form exists to prevent is a site saved with a
 * credential that does not work and nobody finding out until an editor opens
 * the Media Library. It reports the two failures separately — unreachable and
 * refused — since one is a URL or a firewall and the other is a credential, and
 * conflating them sends an operator to check the wrong thing.
 *
 * Neither failure blocks the save. An operator configuring a site before the
 * library exists, or during an outage, has a legitimate reason to store the
 * settings anyway; refusing would make this form unusable in exactly the
 * situations where somebody is trying to fix things.
 */
final class SettingsForm extends ConfigFormBase {

  public function __construct(private readonly Client $client) {}

  /**
   * {@inheritdoc}
   */
  public static function create(ContainerInterface $container): static {
    $form = new static($container->get('damrs.client'));
    // ConfigFormBase's own dependencies come from the container rather than
    // through the constructor, which is what the base class expects of a
    // subclass that adds its own.
    $form->setConfigFactory($container->get('config.factory'));
    $form->setMessenger($container->get('messenger'));
    $form->setStringTranslation($container->get('string_translation'));

    return $form;
  }

  /**
   * {@inheritdoc}
   */
  public function getFormId(): string {
    return 'damrs_settings';
  }

  /**
   * {@inheritdoc}
   */
  protected function getEditableConfigNames(): array {
    return ['damrs.settings'];
  }

  /**
   * {@inheritdoc}
   */
  public function buildForm(array $form, FormStateInterface $form_state): array {
    $config = $this->config('damrs.settings');

    $form['connection'] = [
      '#type' => 'details',
      '#title' => $this->t('Connection'),
      '#open' => TRUE,
    ];
    $form['connection']['base_url'] = [
      '#type' => 'url',
      '#title' => $this->t('damrs base URL'),
      '#default_value' => $config->get('base_url'),
      '#required' => TRUE,
      '#description' => $this->t('For example https://dam.example.com. No trailing slash needed.'),
    ];
    $form['connection']['tenant_id'] = [
      '#type' => 'textfield',
      '#title' => $this->t('Tenant'),
      '#default_value' => $config->get('tenant_id'),
      '#required' => TRUE,
      '#description' => $this->t('The tenant this site draws from. Signed into every delivery URL, so a wrong value produces URLs damrs refuses rather than assets from the wrong library.'),
    ];
    $form['connection']['api_key'] = [
      '#type' => 'textfield',
      '#title' => $this->t('Service-account API key'),
      '#default_value' => $config->get('api_key'),
      '#description' => $this->t('Used for editorial screens only. Rendering a page never uses it.'),
    ];

    $form['signing'] = [
      '#type' => 'details',
      '#title' => $this->t('Delivery URL signing'),
      '#open' => TRUE,
      '#description' => $this->t('Transform URLs are signed here, in PHP, with no call to damrs — so an outage upstream leaves pages stale rather than blank.'),
    ];
    $form['signing']['signing_key_id'] = [
      '#type' => 'textfield',
      '#title' => $this->t('Signing key id'),
      '#default_value' => $config->get('signing_key_id'),
    ];
    $form['signing']['signing_secret'] = [
      '#type' => 'textfield',
      '#title' => $this->t('Signing secret'),
      '#default_value' => $config->get('signing_secret'),
    ];
    $form['signing']['previous_signing_secret'] = [
      '#type' => 'textfield',
      '#title' => $this->t('Previous signing secret'),
      '#default_value' => $config->get('previous_signing_secret'),
      '#description' => $this->t('Kept during a rotation. This site decides when to switch, and damrs accepts either secret under the same key id in the meantime — otherwise rotating would break every URL already rendered into a cached page.'),
    ];

    $form['delivery'] = [
      '#type' => 'details',
      '#title' => $this->t('Delivery context'),
      '#open' => FALSE,
    ];
    $form['delivery']['channel'] = [
      '#type' => 'textfield',
      '#title' => $this->t('Channel'),
      '#default_value' => $config->get('channel') ?: 'web',
      '#description' => $this->t('Reported to damrs so licence terms that differ by channel are evaluated correctly.'),
    ];
    $form['delivery']['territory'] = [
      '#type' => 'textfield',
      '#title' => $this->t('Territory'),
      '#default_value' => $config->get('territory'),
      '#description' => $this->t('Two-letter code, or blank for no territory restriction.'),
    ];
    $form['delivery']['url_ttl'] = [
      '#type' => 'number',
      '#title' => $this->t('Signed URL lifetime (seconds)'),
      '#default_value' => $config->get('url_ttl') ?: 3600,
      '#min' => 60,
      '#description' => $this->t('Set this longer than the page cache lifetime, or a cached page will hold URLs that have already expired.'),
    ];

    return parent::buildForm($form, $form_state);
  }

  /**
   * {@inheritdoc}
   */
  public function submitForm(array &$form, FormStateInterface $form_state): void {
    $this->config('damrs.settings')
      ->set('base_url', rtrim((string) $form_state->getValue('base_url'), '/'))
      ->set('tenant_id', $form_state->getValue('tenant_id'))
      ->set('api_key', $form_state->getValue('api_key'))
      ->set('signing_key_id', $form_state->getValue('signing_key_id'))
      ->set('signing_secret', $form_state->getValue('signing_secret'))
      ->set('previous_signing_secret', $form_state->getValue('previous_signing_secret'))
      ->set('channel', $form_state->getValue('channel'))
      ->set('territory', $form_state->getValue('territory'))
      ->set('url_ttl', (int) $form_state->getValue('url_ttl'))
      ->save();

    parent::submitForm($form, $form_state);

    // Checked after saving, against what was just stored, so the message
    // describes the configuration the site now has rather than the one that was
    // in the form.
    if (!$this->client->reachable()) {
      $this->messenger()->addWarning($this->t('Saved, but damrs did not answer at that URL. The settings are stored; check the URL and that this site can reach it.'));

      return;
    }
    if (!$this->client->checkCredential()) {
      $this->messenger()->addWarning($this->t('Saved, and damrs answered — but it refused the API key. The settings are stored; check the service account.'));

      return;
    }
    $this->messenger()->addStatus($this->t('Saved, and damrs accepted the credential.'));
  }

}
