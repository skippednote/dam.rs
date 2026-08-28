<?php

declare(strict_types=1);

namespace Drupal\damrs;

/**
 * What came back from one damrs request, including nothing.
 *
 * `Client::asset()` returns NULL for both "damrs did not answer" and "damrs
 * answered that there is no such asset", and for most callers that is fine —
 * both mean render the fallback. For anything deciding whether to *retry*, the
 * two are opposites: an unreachable damrs is weather and will pass, and a
 * refused or missing asset will still be refused or missing on the tenth
 * attempt.
 *
 * The sync module needs that distinction and cannot get it from NULL. Treating
 * every failure as retryable makes a deleted asset a queue item that never
 * drains; treating every failure as final makes a one-minute outage erase the
 * metadata it could not refresh.
 */
final class ApiResult {

  public function __construct(
    public readonly ?array $data,
    /**
     * The HTTP status, or NULL when no response arrived at all.
     */
    public readonly ?int $status,
  ) {}

  /**
   * Whether damrs answered at all.
   *
   * @return bool
   *   TRUE when no response arrived — a timeout, a refused connection, DNS.
   */
  public function unreachable(): bool {
    return $this->status === NULL;
  }

  /**
   * Whether damrs answered and the answer was usable.
   *
   * @return bool
   *   TRUE for a 2xx with a decoded body.
   */
  public function ok(): bool {
    return $this->data !== NULL;
  }

  /**
   * Whether damrs answered that this is not available to us.
   *
   * 404 and 403 together, because they mean the same thing to a caller deciding
   * whether to retry: no number of attempts changes either. They differ for a
   * person reading a log, which is why the status is kept.
   *
   * @return bool
   *   TRUE when damrs answered with a refusal that retrying will not fix.
   */
  public function refused(): bool {
    return $this->status === 404 || $this->status === 403;
  }

}
