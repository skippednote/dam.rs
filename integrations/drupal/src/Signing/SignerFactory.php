<?php

declare(strict_types=1);

namespace Drupal\damrs\Signing;

use Drupal\Component\Datetime\TimeInterface;
use Drupal\Core\Config\ConfigFactoryInterface;

/**
 * Builds a signer, and builds the delivery URLs the render path uses.
 *
 * Separate from the client on purpose: the client talks to damrs over HTTP and can fail, and this cannot.
 * Signing is arithmetic over a local secret, so the render path depends on this class and never on the
 * client — which is what §11.3 means by a damrs outage degrading to stale-but-working pages.
 */
final class SignerFactory {

  public function __construct(
    private readonly ConfigFactoryInterface $configFactory,
    // Injected rather than reached for through \Drupal::time(). A static call here would make this class
    // untestable without a container, and the signer is the one part of the module that most wants a plain
    // unit test.
    private readonly TimeInterface $time,
  ) {}

  /**
   * A signer using the current secret.
   */
  public function signer(): Signer {
    $config = $this->configFactory->get('damrs.settings');
    $secret = (string) $config->get('signing_secret');
    if ($secret === '') {
      throw new \RuntimeException('damrs has no signing secret configured; delivery URLs cannot be signed');
    }

    return new Signer($secret);
  }

  /**
   * The URL to render for one asset and transform.
   *
   * The expiry comes from configuration rather than from the caller. A per-call TTL sounds flexible and is
   * a footgun: a long one weakens revocation, a short one expires inside a page cache, and the correct
   * value depends on the site's cache lifetime rather than on the template asking for the image.
   */
  public function deliveryUrl(string $assetId, string $transform, ?string $identityId = NULL): string {
    $config = $this->configFactory->get('damrs.settings');
    $base = rtrim((string) $config->get('base_url'), '/');
    $ttl = (int) ($config->get('url_ttl') ?: 3600);

    $claim = new DeliveryClaim(
      tenantId: (string) $config->get('tenant_id'),
      assetId: $assetId,
      transform: $transform,
      channel: (string) ($config->get('channel') ?: 'web'),
      territory: (string) ($config->get('territory') ?: ''),
      // Computed here rather than passed in, so every URL from this site has the same lifetime and a
      // caller cannot accidentally mint a long-lived one.
      expiresAt: $this->time->getRequestTime() + $ttl,
      keyId: (string) $config->get('signing_key_id'),
      identityId: $identityId,
    );

    return $base . '/d/' . $this->signer()->sign($claim);
  }

}
