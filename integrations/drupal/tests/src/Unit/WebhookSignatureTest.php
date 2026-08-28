<?php

declare(strict_types=1);

namespace Drupal\Tests\damrs\Unit;

use Drupal\damrs\WebhookSignature;
use PHPUnit\Framework\Attributes\CoversClass;
use PHPUnit\Framework\Attributes\DataProvider;
use PHPUnit\Framework\Attributes\Group;
use PHPUnit\Framework\TestCase;

/**
 * Holds the webhook verifier to damrs, and to the forgeries it must refuse.
 *
 * The vectors come from `cargo run -p dam-connect --example webhook_vectors`.
 * Unlike the delivery-token vectors, the important half here is the rejections:
 * this endpoint is reachable without a session, so a verifier wrong in the
 * accepting direction is an endpoint anybody can post content changes to. A
 * suite built from the happy path alone would pass on an implementation that
 * signs the body without the timestamp — which makes every signature a
 * permanent replay token.
 */
#[Group('damrs')]
#[CoversClass(WebhookSignature::class)]
final class WebhookSignatureTest extends TestCase {

  /**
   * The vectors, decoded.
   *
   * @return array
   *   The decoded webhook_vectors.json.
   */
  private static function fixture(): array {
    $path = __DIR__ . '/../../fixtures/webhook_vectors.json';
    $raw = file_get_contents($path);
    if ($raw === FALSE) {
      throw new \RuntimeException("missing $path; regenerate it with "
        . 'cargo run -p dam-connect --example webhook_vectors');
    }

    return json_decode($raw, TRUE, 512, JSON_THROW_ON_ERROR);
  }

  /**
   * The signature damrs produces is accepted.
   */
  public function testTheSignatureDamrsProducesIsAccepted(): void {
    $v = self::fixture();

    self::assertTrue((new WebhookSignature())->isValid(
      $v['secret'],
      $v['valid_signature'],
      (string) $v['timestamp'],
      $v['body'],
      // Inside the window by construction.
      $v['timestamp'],
    ));
  }

  /**
   * Every forgery in the fixture is refused.
   *
   * @return array
   *   One case per forgery, keyed by what makes it a forgery.
   */
  public static function forgeries(): array {
    $v = self::fixture();
    $out = [];
    foreach ($v['must_reject'] as $case) {
      $out[$case['why']] = [
        $v['secret'],
        $case['signature'],
        (string) $case['timestamp'],
        $case['body'],
        $v['timestamp'],
      ];
    }

    return $out;
  }

  /**
   * A forgery is refused whatever shape it takes.
   */
  #[DataProvider('forgeries')]
  public function testForgeriesAreRefused(string $secret, string $signature, string $timestamp, string $body, int $now): void {
    self::assertFalse(
      (new WebhookSignature())->isValid($secret, $signature, $timestamp, $body, $now),
    );
  }

  /**
   * A correctly signed but stale delivery is refused.
   *
   * A signature proves origin, not freshness. Without a window, one captured
   * signature is a lasting credential for whoever captured it.
   */
  public function testStaleDeliveryIsRefused(): void {
    $v = self::fixture();
    $verifier = new WebhookSignature();

    $edge = $v['timestamp'] + WebhookSignature::TOLERANCE;
    self::assertTrue(
      $verifier->isValid($v['secret'], $v['valid_signature'], (string) $v['timestamp'], $v['body'], $edge),
      'the boundary itself is still inside the window',
    );
    self::assertFalse(
      $verifier->isValid($v['secret'], $v['valid_signature'], (string) $v['timestamp'], $v['body'], $edge + 1),
      'one second past it is not',
    );
    // Both directions: a delivery from the future is as suspect as a stale one,
    // and a receiver checking only one side accepts a signature minted with a
    // far-future timestamp and usable until then.
    self::assertFalse(
      $verifier->isValid($v['secret'], $v['valid_signature'], (string) $v['timestamp'], $v['body'], $v['timestamp'] - WebhookSignature::TOLERANCE - 1),
    );
  }

  /**
   * A non-numeric timestamp header is refused rather than coerced.
   *
   * The timestamp is half the signed string. `(int) "1800000000junk"` is
   * 1800000000, so coercing would let two different headers produce one signed
   * string — and a receiver echoing the header back would disagree with what it
   * verified.
   */
  #[DataProvider('badTimestamps')]
  public function testNonNumericTimestampIsRefused(string $header): void {
    $v = self::fixture();

    self::assertFalse((new WebhookSignature())->isValid(
      $v['secret'],
      $v['valid_signature'],
      $header,
      $v['body'],
      $v['timestamp'],
    ));
  }

  /**
   * Timestamp headers that must not be accepted.
   *
   * @return array
   *   One header per case, keyed by what is wrong with it.
   */
  public static function badTimestamps(): array {
    return [
      'empty' => [''],
      'trailing junk' => ['1800000000junk'],
      'leading space' => [' 1800000000'],
      'hex' => ['0x6b49d200'],
      'float' => ['1800000000.0'],
      'words' => ['now'],
    ];
  }

  /**
   * The comparison is constant-time.
   *
   * Checked structurally, because it cannot be checked behaviourally.
   *
   * Replacing `hash_equals` with `===` passes every other test in this class —
   * the accept and reject decisions are identical, and only the *timing*
   * differs. A comparison that returns on the first differing byte leaks the
   * prefix of a valid signature to anyone who can measure the response, and
   * with that oracle a forgery is arithmetic rather than luck.
   *
   * So this reads the source. That is a blunt instrument and it is the only one
   * available: the property is real, the consequence is a forgeable endpoint,
   * and a mutation run confirmed nothing else here notices. Better a structural
   * assertion that says what it is than a silent gap.
   */
  public function testTheComparisonIsConstantTime(): void {
    $source = file_get_contents(__DIR__ . '/../../../src/WebhookSignature.php');
    self::assertNotFalse($source);

    self::assertStringContainsString('hash_equals(', $source);
    self::assertDoesNotMatchRegularExpression(
      '/\$expected\s*===/',
      $source,
      'comparing the digest with === leaks its prefix through response timing',
    );
  }

  /**
   * An empty secret refuses everything.
   *
   * A site that has not configured the secret must not accept deliveries, and
   * the failure mode of an empty HMAC key is a valid-looking digest rather than
   * an error — so this is checked rather than assumed.
   */
  public function testEmptySecretRefuses(): void {
    $v = self::fixture();

    self::assertFalse((new WebhookSignature())->isValid(
      '',
      $v['valid_signature'],
      (string) $v['timestamp'],
      $v['body'],
      $v['timestamp'],
    ));
  }

}
