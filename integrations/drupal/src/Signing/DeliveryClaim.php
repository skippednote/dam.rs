<?php

declare(strict_types=1);

namespace Drupal\damrs\Signing;

/**
 * One delivery token's payload, before it is signed.
 *
 * A value object rather than an array, because the canonical form is positional: getting two fields the
 * wrong way round produces a token that signs and verifies as something else, and an array with string keys
 * hides that behind names the encoder ignores.
 *
 * The field list and its order are fixed by damrs and documented in `dam_core::signed_url`. Anything here
 * that disagrees with that module is a bug in this file.
 */
final class DeliveryClaim {

  /**
   * A URL that will hand out the bytes to somebody outside.
   *
   * Rights are evaluated in full at delivery.
   */
  public const PURPOSE_DISTRIBUTION = 'distribution';

  /**
   * A URL for looking at the asset inside the application.
   *
   * A different purpose byte, and not interchangeable: damrs refuses a token whose purpose does not match
   * what the route is for, so signing a download as a preview is a refusal rather than a leak.
   */
  public const PURPOSE_INTERNAL_PREVIEW = 'internal_preview';

  public function __construct(
    public readonly string $tenantId,
    public readonly string $assetId,
    public readonly string $transform,
    public readonly string $channel,
    public readonly string $territory,
    public readonly int $expiresAt,
    public readonly string $keyId,
    public readonly string $purpose = self::PURPOSE_DISTRIBUTION,
    public readonly ?string $identityId = NULL,
    public readonly ?string $shareLinkId = NULL,
  ) {}

}
