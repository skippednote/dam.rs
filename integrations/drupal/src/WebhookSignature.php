<?php

declare(strict_types=1);

namespace Drupal\damrs;

/**
 * Verifies that a webhook really came from damrs.
 *
 * This class is the whole security boundary of the sync module. The endpoint it
 * guards has to be reachable by damrs without a session, which means it is
 * reachable by anybody, and everything behind it edits content. A verifier
 * wrong in the accepting direction is not a bug in a feature — it is an
 * endpoint the internet can post to.
 *
 * ## The timestamp is inside the signature, and that is the point
 *
 * The signed string is `{timestamp}.{body}`, not the body. A signature over the
 * body alone would be valid forever, so anybody who ever saw one delivery could
 * replay it — re-applying an `asset.deleted` after the republication that
 * followed it, for instance. Verifying the body alone is the single most likely
 * way to write this wrongly and it passes every happy-path test, which is why
 * `tests/fixtures/webhook_vectors.json` carries that exact forgery and requires
 * it to be rejected.
 *
 * ## Old is rejected even when correctly signed
 *
 * A correct signature proves origin, not freshness. Without a window, a
 * signature captured once is usable indefinitely by whoever captured it, so a
 * delivery older than [self::TOLERANCE] is refused however well signed.
 *
 * ## Comparison is constant-time
 *
 * `hash_equals`, not `===`. A comparison that returns on the first differing
 * byte leaks the prefix of a valid signature to anyone who can measure the
 * response, and with an oracle that is a forgery rather than a nuisance.
 */
final class WebhookSignature {

  /**
   * The scheme version this class understands.
   *
   * Pinned rather than parsed loosely, so that a future `v2=` arriving beside
   * `v1=` is rejected by an old receiver instead of being half-understood.
   */
  private const VERSION = 'v1';

  /**
   * How far out of date a delivery may be, in seconds.
   *
   * Five minutes: long enough for a queued retry and a clock a little out of
   * step, short enough that a captured signature is not a lasting credential.
   */
  public const TOLERANCE = 300;

  /**
   * Whether this delivery is genuinely from damrs and fresh enough to apply.
   *
   * @param string $secret
   *   The subscription's shared secret.
   * @param string $presented
   *   The value of the X-Damrs-Signature header.
   * @param string $timestampHeader
   *   The value of the X-Damrs-Timestamp header, as it arrived.
   * @param string $body
   *   The raw request body, byte for byte as received.
   * @param int $now
   *   The current time, in seconds since the epoch.
   *
   * @return bool
   *   TRUE only if the signature matches and the delivery is inside the window.
   */
  public function isValid(string $secret, string $presented, string $timestampHeader, string $body, int $now): bool {
    if ($secret === '' || $presented === '') {
      return FALSE;
    }

    // A strictly numeric timestamp, because it is half of the signed string.
    // `(int) "1800000000abc"` is 1800000000, and accepting that would let two
    // different headers produce one signed string.
    if ($timestampHeader === '' || preg_match('/^-?[0-9]+$/', $timestampHeader) !== 1) {
      return FALSE;
    }
    $timestamp = (int) $timestampHeader;

    if (abs($now - $timestamp) > self::TOLERANCE) {
      return FALSE;
    }

    $prefix = self::VERSION . '=';
    if (!str_starts_with($presented, $prefix)) {
      return FALSE;
    }

    $expected = $prefix . hash_hmac('sha256', $timestamp . '.' . $body, $secret);

    return hash_equals($expected, $presented);
  }

}
