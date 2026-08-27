<?php

declare(strict_types=1);

namespace Drupal\damrs_sync\Plugin\QueueWorker;

use Drupal\Core\Config\ConfigFactoryInterface;
use Drupal\Core\Plugin\ContainerFactoryPluginInterface;
use Drupal\Core\Queue\Attribute\QueueWorker;
use Drupal\Core\Queue\QueueWorkerBase;
use Drupal\Core\Queue\SuspendQueueException;
use Drupal\Core\StringTranslation\TranslatableMarkup;
use Drupal\Core\Entity\EntityStorageException;
use Drupal\damrs_sync\EventApplier;
use Drupal\damrs_sync\UnreachableException;
use Psr\Log\LoggerInterface;
use Symfony\Component\DependencyInjection\ContainerInterface;

/**
 * Drains the queue of verified damrs events.
 *
 * Every item here has already been authenticated by the controller, so this
 * worker's only job is to apply one and decide what a failure means.
 *
 * ## What throwing means, and why most failures do not
 *
 * A thrown exception leaves the item on the queue for the next cron run. That
 * is right for a transient failure and wrong for a malformed item, which would
 * be retried forever — so an item that can never succeed is logged and dropped.
 * `SuspendQueueException` is for the case where continuing is pointless: if the
 * media storage is unavailable, the next item will fail the same way, and
 * hammering it for the rest of the cron run just fills the log.
 */
#[QueueWorker(
  id: 'damrs_sync_events',
  title: new TranslatableMarkup('damrs events'),
  // 30 seconds per cron run. Deliberately modest: this competes with
  // everything else cron does, and a large backlog is better drained over
  // several runs than by starving the rest of them.
  cron: ['time' => 30],
)]
final class EventWorker extends QueueWorkerBase implements ContainerFactoryPluginInterface {

  public function __construct(
    array $configuration,
    $plugin_id,
    $plugin_definition,
    private readonly EventApplier $applier,
    private readonly ConfigFactoryInterface $config,
    private readonly LoggerInterface $logger,
  ) {
    parent::__construct($configuration, $plugin_id, $plugin_definition);
  }

  /**
   * {@inheritdoc}
   */
  public static function create(ContainerInterface $container, array $configuration, $plugin_id, $plugin_definition): static {
    return new static(
      $configuration,
      $plugin_id,
      $plugin_definition,
      $container->get('damrs_sync.applier'),
      $container->get('config.factory'),
      $container->get('logger.channel.damrs'),
    );
  }

  /**
   * {@inheritdoc}
   */
  public function processItem($data): void {
    if (!is_array($data) || !isset($data['event'], $data['asset_id'])) {
      // Dropped rather than retried: nothing about a later attempt makes an
      // item without an event name valid.
      $this->logger->error('discarding a damrs queue item with no event or asset id');

      return;
    }

    $unpublish = (bool) $this->config->get('damrs_sync.settings')->get('unpublish_on_delete');

    try {
      $touched = $this->applier->apply(
        (string) $data['event'],
        (string) $data['asset_id'],
        $unpublish,
      );
    }
    catch (UnreachableException $e) {
      // damrs is not answering. Every remaining item would fail the same way,
      // and each failure would be an entity load and an HTTP timeout, so the
      // run stops here rather than working through the backlog to discover the
      // same thing repeatedly. The items stay queued and the next cron retries.
      throw new SuspendQueueException($e->getMessage(), 0, $e);
    }
    catch (EntityStorageException $e) {
      // Storage said no, and for the same reason as above there is nothing to
      // gain from trying the rest.
      throw new SuspendQueueException($e->getMessage(), 0, $e);
    }

    if ($touched > 0) {
      $this->logger->info('applied @event to @count media item(s) for @asset', [
        '@event' => $data['event'],
        '@count' => $touched,
        '@asset' => $data['asset_id'],
      ]);
    }
  }

}
