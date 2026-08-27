<?php

declare(strict_types=1);

namespace Drupal\damrs;

use Drupal\Core\Config\ConfigFactoryInterface;
use GuzzleHttp\ClientInterface;
use GuzzleHttp\Exception\GuzzleException;
use Psr\Log\LoggerInterface;

/**
 * Talks to damrs.
 *
 * Everything here can fail, and nothing here is on the render path. That
 * division is deliberate: transform URLs are signed locally by
 * \Drupal\damrs\Signing\SignerFactory, so a damrs outage cannot white-screen a
 * page. This class is for the editorial surfaces — picking an asset, refreshing
 * cached metadata, checking the connection from the settings form — where
 * waiting on an API is the expected behaviour and a failure has somewhere
 * sensible to be reported.
 *
 * ## Failures are values, not exceptions
 *
 * Methods return NULL or an empty result and log. A media entity rendering a
 * stale cached title is the correct outcome of a damrs outage; an uncaught
 * exception from a field formatter is not.
 */
final class Client {

  /**
   * How long to wait on damrs before giving up.
   *
   * Short, because every caller has a person waiting on it. A generous timeout
   * on an editorial screen just means a longer spinner before the same failure.
   */
  private const TIMEOUT = 10;

  public function __construct(
    private readonly ClientInterface $httpClient,
    private readonly ConfigFactoryInterface $configFactory,
    private readonly LoggerInterface $logger,
  ) {}

  /**
   * Whether damrs answers at all.
   *
   * `/health` is unauthenticated and says nothing beyond 200, deliberately — it
   * is the first thing anybody scans, so it reports no version and no tenant
   * state. That makes it a reachability check and not a credential check, which
   * is why [self::checkCredential] exists separately.
   */
  public function reachable(): bool {
    $response = $this->request('GET', '/health', authenticated: FALSE);

    return $response !== NULL;
  }

  /**
   * Whether the configured service account is accepted.
   *
   * Separate from [self::reachable] because the two failures have different
   * fixes and a settings form that conflated them would send an operator to
   * check the wrong thing: an unreachable host is a URL or a firewall, and a
   * refused key is a credential.
   */
  public function checkCredential(): bool {
    // Any authenticated route answers this; browse is the cheapest that needs
    // no id. A page of one, because the question is whether the key is accepted
    // rather than what is in the library.
    $response = $this->request('GET', '/browse?limit=1');

    return $response !== NULL;
  }

  /**
   * One asset's metadata, or NULL if damrs could not answer.
   */
  public function asset(string $assetId): ?array {
    return $this->request('GET', '/assets/' . urlencode($assetId));
  }

  /**
   * A page of the library, for the Media Library picker.
   *
   * @param array $query
   *   Query parameters for `GET /browse`: the search text, paging and facets.
   *
   * @return array
   *   The decoded response, or an empty array when damrs could not answer.
   *   Empty rather than NULL because every caller renders a list, and an empty
   *   list is the honest degraded view.
   */
  public function browse(array $query = []): array {
    $path = '/browse' . ($query === [] ? '' : '?' . http_build_query($query));

    return $this->request('GET', $path) ?? [];
  }

  /**
   * Issues one request and decodes it.
   *
   * Logs and returns NULL on any failure, so a caller never has to catch.
   *
   * @return array|null
   *   The decoded body, or NULL if damrs could not answer.
   */
  private function request(string $method, string $path, bool $authenticated = TRUE): ?array {
    $config = $this->configFactory->get('damrs.settings');
    $base = rtrim((string) $config->get('base_url'), '/');
    if ($base === '') {
      $this->logger->warning('damrs has no base URL configured; skipping @method @path', [
        '@method' => $method,
        '@path' => $path,
      ]);

      return NULL;
    }

    $options = [
      'timeout' => self::TIMEOUT,
      'headers' => ['Accept' => 'application/json'],
      // Errors are handled below from the status code rather than by Guzzle
      // throwing, so a 403 and a connection failure take the same path and get
      // the same logging.
      'http_errors' => FALSE,
    ];
    if ($authenticated) {
      $key = (string) $config->get('api_key');
      if ($key === '') {
        $this->logger->warning('damrs has no API key configured; skipping @path', ['@path' => $path]);

        return NULL;
      }
      $options['headers']['Authorization'] = 'Bearer ' . $key;
    }

    try {
      $response = $this->httpClient->request($method, $base . $path, $options);
    }
    catch (GuzzleException $e) {
      // The message, not the exception: a Guzzle exception's string form can
      // carry the request headers, and those headers hold the service-account
      // key.
      $this->logger->error('damrs did not answer @path: @reason', [
        '@path' => $path,
        '@reason' => $e->getMessage(),
      ]);

      return NULL;
    }

    $status = $response->getStatusCode();
    if ($status < 200 || $status >= 300) {
      $this->logger->error('damrs answered @status for @path', [
        '@status' => $status,
        '@path' => $path,
      ]);

      return NULL;
    }

    $body = (string) $response->getBody();
    if ($body === '') {
      // A 200 with no body is a valid answer from /health, and there is nothing
      // to decode.
      return [];
    }

    try {
      $decoded = json_decode($body, TRUE, 512, JSON_THROW_ON_ERROR);
    }
    catch (\JsonException $e) {
      $this->logger->error('damrs answered @path with something that is not JSON: @reason', [
        '@path' => $path,
        '@reason' => $e->getMessage(),
      ]);

      return NULL;
    }

    return is_array($decoded) ? $decoded : [];
  }

}
