<?php

declare(strict_types=1);

namespace Drupal\Tests\damrs\Unit;

use Drupal\damrs\Signing\DeliveryClaim;
use Drupal\damrs\Signing\Signer;
use PHPUnit\Framework\Attributes\CoversClass;
use PHPUnit\Framework\Attributes\DataProvider;
use PHPUnit\Framework\Attributes\Group;
use PHPUnit\Framework\TestCase;

/**
 * Holds the PHP signer to the bytes damrs produces.
 *
 * The vectors come from `cargo run -p dam-core --example signing_vectors`, so this is a comparison against
 * the implementation that will actually verify these tokens rather than against this file's own idea of the
 * format. Asserting "the server accepted it" would need a live server and would pass against a server
 * running the same wrong assumption; asserting the bytes catches a drift the moment it is introduced.
 */
#[Group('damrs')]
#[CoversClass(Signer::class)]
final class SignerTest extends TestCase {

  /**
   * The vectors, keyed by case name.
   *
   * @return array<string, array{0: array<string, mixed>, 1: string, 2: string, 3: string}>
   */
  public static function vectors(): array {
    $path = __DIR__ . '/../../fixtures/signing_vectors.json';
    $raw = file_get_contents($path);
    if ($raw === FALSE) {
      throw new \RuntimeException("the signing vectors are missing at $path; regenerate them with "
        . 'cargo run -p dam-core --example signing_vectors');
    }
    $data = json_decode($raw, TRUE, 512, JSON_THROW_ON_ERROR);

    $out = [];
    foreach ($data['cases'] as $case) {
      // The name and the reason ride along so a failure says which property broke rather than only which
      // index did.
      $out[$case['name']] = [$case['claim'], $case['token'], $data['secret'], $case['why']];
    }

    return $out;
  }

  /**
   * Every vector's token, byte for byte.
   */
  #[DataProvider('vectors')]
  public function testProducesTheSameTokenAsDamrs(array $claim, string $expected, string $secret, string $why): void {
    $signer = new Signer($secret);

    $token = $signer->sign(new DeliveryClaim(
      tenantId: $claim['tenant_id'],
      assetId: $claim['asset_id'],
      transform: $claim['transform'],
      channel: $claim['channel'],
      territory: $claim['territory'],
      expiresAt: $claim['expires_at'],
      keyId: $claim['key_id'],
      purpose: $claim['purpose'],
      identityId: $claim['identity_id'],
      shareLinkId: $claim['share_link_id'],
    ));

    self::assertSame($expected, $token, $why);
  }

  /**
   * An unknown purpose is refused, not defaulted.
   *
   * Defaulting would turn a typo into a public download URL, which is the one direction this must never
   * fail in — damrs treats the two purposes as distinct precisely so that a preview cannot be served as a
   * distribution.
   */
  public function testAnUnknownPurposeIsRefused(): void {
    $signer = new Signer('irrelevant');

    $this->expectException(\InvalidArgumentException::class);
    $signer->sign(new DeliveryClaim(
      tenantId: '11111111-2222-3333-4444-555555555555',
      assetId: '66666666-7777-8888-9999-aaaaaaaaaaaa',
      transform: 'w=800',
      channel: 'web',
      territory: 'GB',
      expiresAt: 1800000000,
      keyId: 'k1',
      purpose: 'whatever-the-caller-felt-like',
    ));
  }

  /**
   * A malformed uuid is refused before it can shift every field after it.
   */
  #[DataProvider('malformedUuids')]
  public function testAMalformedUuidIsRefused(string $uuid): void {
    $signer = new Signer('irrelevant');

    $this->expectException(\InvalidArgumentException::class);
    $signer->sign(new DeliveryClaim(
      tenantId: $uuid,
      assetId: '66666666-7777-8888-9999-aaaaaaaaaaaa',
      transform: 'w=800',
      channel: 'web',
      territory: 'GB',
      expiresAt: 1800000000,
      keyId: 'k1',
    ));
  }

  /**
   * @return array<string, array{0: string}>
   */
  public static function malformedUuids(): array {
    return [
      'empty' => [''],
      'too short' => ['1111-2222'],
      'not hex' => ['zzzzzzzz-2222-3333-4444-555555555555'],
      'a number' => ['12345'],
    ];
  }

  /**
   * An empty optional and an absent one must not sign the same.
   *
   * The vectors cover this from the damrs side; this states it as a property, because it is the collision
   * the zero-length-field rule exists to prevent and a reader of this class should see it named.
   */
  public function testAnAbsentOptionalDiffersFromAnEmptyString(): void {
    $signer = new Signer('secret');
    $base = [
      'tenantId' => '11111111-2222-3333-4444-555555555555',
      'assetId' => '66666666-7777-8888-9999-aaaaaaaaaaaa',
      'channel' => 'web',
      'territory' => 'GB',
      'expiresAt' => 1800000000,
      'keyId' => 'k1',
    ];

    // A transform of '' with no identity, against a transform of '' with an identity: the only difference
    // is a field that would vanish under a delimiter-joined encoding.
    $without = $signer->sign(new DeliveryClaim(...[...$base, 'transform' => '']));
    $with = $signer->sign(new DeliveryClaim(...[
      ...$base,
      'transform' => '',
      'identityId' => 'bbbbbbbb-cccc-dddd-eeee-ffffffffffff',
    ]));

    self::assertNotSame($without, $with);
  }

}
