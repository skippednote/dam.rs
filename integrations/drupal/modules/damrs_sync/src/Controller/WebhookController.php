<?php

declare(strict_types=1);

namespace Drupal\damrs_sync\Controller;

use Drupal\Component\Datetime\TimeInterface;
use Drupal\Core\Config\ConfigFactoryInterface;
use Drupal\Core\Controller\ControllerBase;
use Drupal\Core\Queue\QueueFactory;
use Drupal\damrs\WebhookSignature;
use Psr\Log\LoggerInterface;
use Symfony\Component\DependencyInjection\ContainerInterface;
use Symfony\Component\HttpFoundation\JsonResponse;
use Symfony\Component\HttpFoundation\Request;

/**
 * Receives a damrs webhook, verifies it, and queues it.
 *
 * ## Queued rather than applied
 *
 * The response says "accepted", not "done". damrs waits ten seconds and then
 * retries, and applying an event inline would mean a slow re-save turning into
 * a duplicate delivery — while a bulk publication of ten thousand assets would
 * arrive as ten thousand requests each doing entity work in the request
 * thread. So the endpoint verifies and enqueues, and cron drains.
 *
 * The consequence is worth being explicit about: an accepted delivery has not
 * been applied yet, and a queue that stops being drained is a site whose
 * metadata quietly goes stale. That is the right trade — the alternative is an
 * endpoint that times out under load and a retry storm behind it — but it does
 * mean the queue needs to be running, not merely configured.
 *
 * ## Verified before anything else is read
 *
 * The route has no access requirement, because damrs has no session and no form
 * token; the signature is the entire boundary. So the body is read as raw bytes
 * and verified before it is parsed, and nothing derived from it is used before
 * that check passes.
 *
 * ## What the response says, and does not
 *
 * A bad signature is a flat 401 with no detail. Telling a caller *why* a
 * signature failed — stale, wrong version, wrong digest — is a hint about how
 * to succeed, and this endpoint is reachable by anyone.
 */
final class WebhookController extends ControllerBase {

  /**
   * The queue events are applied from.
   */
  public const QUEUE = 'damrs_sync_events';

  public function __construct(
    private readonly WebhookSignature $signature,
    private readonly ConfigFactoryInterface $config,
    private readonly QueueFactory $queueFactory,
    private readonly TimeInterface $time,
    private readonly LoggerInterface $logger,
  ) {}

  /**
   * {@inheritdoc}
   */
  public static function create(ContainerInterface $container): static {
    return new static(
      new WebhookSignature(),
      $container->get('config.factory'),
      $container->get('queue'),
      $container->get('datetime.time'),
      $container->get('logger.channel.damrs'),
    );
  }

  /**
   * Handles one delivery.
   */
  public function receive(Request $request): JsonResponse {
    $secret = (string) $this->config->get('damrs_sync.settings')->get('webhook_secret');
    if ($secret === '') {
      // Not configured is not the same as forged, and an operator needs to be
      // able to tell those apart from the log. The response is still a flat
      // refusal.
      $this->logger->warning('a damrs webhook arrived but no webhook secret is configured');

      return new JsonResponse(['status' => 'refused'], 401);
    }

    // getContent(), not the parsed body: the signature covers the exact bytes,
    // and anything that re-encoded them would verify something other than what
    // arrived.
    $body = $request->getContent();
    $presented = (string) $request->headers->get('X-Damrs-Signature', '');
    $timestamp = (string) $request->headers->get('X-Damrs-Timestamp', '');

    if (!$this->signature->isValid($secret, $presented, $timestamp, $body, $this->time->getRequestTime())) {
      $this->logger->warning('rejected a damrs webhook whose signature did not verify');

      return new JsonResponse(['status' => 'refused'], 401);
    }

    try {
      $event = json_decode($body, TRUE, 32, JSON_THROW_ON_ERROR);
    }
    catch (\JsonException $e) {
      // Signed by damrs and not JSON is a damrs bug rather than an attack, so
      // it says so — and 400 rather than 401, because retrying will not help.
      $this->logger->error('a verified damrs webhook was not JSON: @reason', ['@reason' => $e->getMessage()]);

      return new JsonResponse(['status' => 'unparseable'], 400);
    }

    if (!is_array($event) || !isset($event['event'], $event['asset_id'])) {
      $this->logger->error('a verified damrs webhook had no event or asset_id');

      return new JsonResponse(['status' => 'incomplete'], 400);
    }

    $this->queueFactory->get(self::QUEUE)->createItem([
      'event' => (string) $event['event'],
      'asset_id' => (string) $event['asset_id'],
      'occurred_at' => (string) ($event['occurred_at'] ?? ''),
      'detail' => $event['detail'] ?? [],
      // Carried so the worker can recognise a retry of something it already
      // did. damrs keeps this stable across attempts for exactly that.
      'delivery' => (string) $request->headers->get('X-Damrs-Delivery', ''),
    ]);

    // 202: queued, not applied. Saying 200 would claim work that has not
    // happened, and damrs's delivery log would record a success for an event
    // still sitting in a queue nobody is draining.
    return new JsonResponse(['status' => 'queued'], 202);
  }

}
