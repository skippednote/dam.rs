<?php

declare(strict_types=1);

namespace Drupal\damrs\Signing;

/**
 * Builds damrs delivery tokens in PHP, with no call to damrs.
 *
 * This is the class §11.3 turns on. Transform URLs are signed locally from the shared secret so that
 * rendering a page never waits on damrs: an outage upstream degrades to stale-but-working pages instead of
 * white screens or a stalled render queue. A CMS integration that has to reach an API to paint a page is
 * not shippable, so the signing has to happen here.
 *
 * ## What the token does and does not do
 *
 * It proves damrs issued this exact request and nothing altered it. It does **not** authorise the bytes —
 * damrs evaluates rights when the URL is fetched. That is what makes expiry in the DAM take effect on a
 * live site: a URL signed this morning stops working this afternoon when the licence lapses, without this
 * module knowing anything about it. It is also why signing locally is safe. A signature that authorised
 * would make every URL this module emits an outstanding grant nobody could withdraw.
 *
 * ## The canonical form is length-prefixed, and that is not a detail
 *
 * Every field is a 32-bit big-endian byte length followed by the bytes. Joining with a delimiter instead
 * would make the encoding ambiguous — `asset=1, transform=ab` and `asset=1a, transform=b` both render
 * `1|ab` — so one signature would cover two different requests, and anyone able to influence any field
 * could forge another. Length prefixes make it injective.
 *
 * Three consequences worth stating, because each is a way a reimplementation goes wrong:
 *
 * - The length counts **bytes**, not characters. `strlen` is correct here and `mb_strlen` is not; a
 *   transform containing `é` would otherwise shift every field after it.
 * - An absent optional field is a field of length zero, **not** an omitted one. Omitting it would shorten
 *   the payload and change what the next length means.
 * - UUIDs are their 16 raw bytes, not their hyphenated text.
 *
 * The bytes this produces are pinned by `tests/fixtures/signing_vectors.json`, generated from the Rust so
 * the two implementations cannot drift silently. If you change anything in `canonical()`, that suite fails.
 */
final class Signer {

  /**
   * The token format version, first byte of the payload.
   *
   * Carried so that a format change is a clean verification failure rather than a misparse of a payload
   * whose fields have shifted. damrs is at 4; a token this module signs as any other version is refused
   * outright, which is the intended behaviour rather than something to work around.
   */
  private const VERSION = 4;

  /**
   * The purpose bytes, which damrs treats as distinct rather than defaulting.
   */
  private const PURPOSE_BYTES = [
    DeliveryClaim::PURPOSE_DISTRIBUTION => 1,
    DeliveryClaim::PURPOSE_INTERNAL_PREVIEW => 2,
  ];

  /**
   * @param string $secret
   *   The shared signing secret. Its raw UTF-8 bytes are the HMAC key.
   */
  public function __construct(private readonly string $secret) {}

  /**
   * Signs a claim and returns the token.
   */
  public function sign(DeliveryClaim $claim): string {
    $payload = $this->canonical($claim);
    $signature = hash_hmac('sha256', $payload, $this->secret, TRUE);

    return $this->base64Url($payload) . '.' . $this->base64Url($signature);
  }

  /**
   * The exact bytes damrs will verify against.
   *
   * Field order is fixed and load-bearing. The purpose comes before the tenant so a reader knows what kind
   * of URL it is holding before anything else; the tenant comes before the asset because an asset id means
   * nothing until the tenant is fixed — and a token that named an asset without a tenant is the
   * cross-tenant bug the version 4 bump exists to close.
   */
  private function canonical(DeliveryClaim $claim): string {
    $purpose = self::PURPOSE_BYTES[$claim->purpose] ?? NULL;
    if ($purpose === NULL) {
      // Refused rather than defaulted. Defaulting to distribution would turn an unrecognised purpose into
      // a public download URL, which is the one direction this must never fail in.
      throw new \InvalidArgumentException(sprintf('unknown delivery purpose "%s"', $claim->purpose));
    }

    $out = chr(self::VERSION);
    $out .= $this->field(chr($purpose));
    $out .= $this->field($this->uuidBytes($claim->tenantId));
    $out .= $this->field($this->uuidBytes($claim->assetId));
    $out .= $this->field($claim->transform);
    $out .= $this->field($claim->channel);
    $out .= $this->field($claim->territory);
    $out .= $this->field($claim->identityId === NULL ? '' : $this->uuidBytes($claim->identityId));
    $out .= $this->field($claim->shareLinkId === NULL ? '' : $this->uuidBytes($claim->shareLinkId));
    $out .= $this->field($this->int64($claim->expiresAt));
    $out .= $this->field($claim->keyId);

    return $out;
  }

  /**
   * One length-prefixed field.
   */
  private function field(string $bytes): string {
    // 'N' is a 32-bit unsigned big-endian integer, matching the u32 damrs writes. strlen, not mb_strlen:
    // the length is in bytes.
    return pack('N', strlen($bytes)) . $bytes;
  }

  /**
   * A signed 64-bit big-endian integer, for the expiry.
   *
   * 'J' is unsigned, and PHP has no signed big-endian pack format — but the two have the same
   * representation in two's complement, and pack() takes the integer's low 64 bits either way. Expiries are
   * positive in any case; this is written explicitly so the next reader does not have to work that out.
   */
  private function int64(int $value): string {
    return pack('J', $value);
  }

  /**
   * A UUID's 16 raw bytes.
   *
   * Not the hyphenated string. damrs signs `Uuid::as_bytes`, so signing the text form would produce a
   * 36-byte field where a 16-byte one belongs and every subsequent field would be misread.
   */
  private function uuidBytes(string $uuid): string {
    $hex = str_replace('-', '', $uuid);
    if (strlen($hex) !== 32 || preg_match('/[^0-9a-fA-F]/', $hex) === 1) {
      throw new \InvalidArgumentException(sprintf('not a uuid: "%s"', $uuid));
    }
    $bytes = hex2bin($hex);
    if ($bytes === FALSE) {
      throw new \InvalidArgumentException(sprintf('not a uuid: "%s"', $uuid));
    }

    return $bytes;
  }

  /**
   * Base64url with no padding, which is what damrs decodes.
   */
  private function base64Url(string $bytes): string {
    return rtrim(strtr(base64_encode($bytes), '+/', '-_'), '=');
  }

}
