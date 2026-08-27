<?php

declare(strict_types=1);

namespace Drupal\damrs_sync;

use Drupal\Core\Entity\EntityTypeManagerInterface;
use Drupal\damrs\Client;
use Drupal\media\MediaInterface;
use Psr\Log\LoggerInterface;

/**
 * Applies one damrs event to whatever media items reference the asset.
 *
 * ## Re-saving is how metadata refreshes, and it is not a workaround
 *
 * `damrs_media` reads metadata in `Media::preSave()`, and Drupal re-reads it
 * only when a mapped field is empty or the source field changed. So an asset
 * retitled in the DAM would never update here on its own — which is the gap
 * this module exists to close, and why `damrs_media` deliberately does not
 * poll.
 *
 * Closing it means clearing the mapped fields and saving, so that Drupal's own
 * "field is empty, read the metadata" branch runs. Blunt, and better than the
 * alternatives: a second metadata-writing path would be the divergence damrs
 * itself keeps finding, and forcing a source-field change would mean writing a
 * value in order to write it back.
 *
 * ## Clearing the fields is only safe once damrs has answered
 *
 * This is the interaction that made the two halves of this connector destroy
 * data together while each was correct alone. `damrs_media` falls back to the
 * value already in the mapped field when damrs cannot answer, precisely so an
 * outage cannot blank cached metadata. Refreshing works by clearing those
 * fields so Drupal re-reads them — which removes the very value the fallback
 * would have returned. A refresh event arriving during an outage therefore
 * erased the metadata it was supposed to update, silently, and both modules'
 * tests were green.
 *
 * So nothing is cleared until damrs has actually produced the asset. If it
 * cannot be reached the item is left for the queue to retry; if it answers that
 * the asset is gone, the cached metadata is kept rather than blanked, because
 * the last thing known about a deleted asset is more useful to an editor than
 * nothing.
 *
 * ## An event names an asset, not a media item
 *
 * One asset can be referenced by several media items, and by none. Both are
 * ordinary: a library is bigger than any one site's use of it, so an event for
 * an asset nobody references is a no-op rather than a warning.
 */
final class EventApplier {

  public function __construct(
    private readonly EntityTypeManagerInterface $entityTypeManager,
    private readonly Client $client,
    private readonly LoggerInterface $logger,
  ) {}

  /**
   * Applies one event.
   *
   * @param string $event
   *   The damrs event name, e.g. `asset.metadata_updated`.
   * @param string $assetId
   *   The asset the event is about.
   * @param bool $unpublishOnDelete
   *   Whether a deletion should unpublish the referencing media items.
   *
   * @return int
   *   How many media items were touched.
   *
   * @throws \Drupal\damrs_sync\UnreachableException
   *   When damrs could not be asked and applying the event would lose data.
   */
  public function apply(string $event, string $assetId, bool $unpublishOnDelete): int {
    $items = $this->referencing($assetId);
    if ($items === []) {
      return 0;
    }

    $touched = 0;
    foreach ($items as $media) {
      $changed = match ($event) {
        // A new version keeps the asset id, so nothing identifies it but the
        // metadata — refreshing is the whole response.
        'asset.metadata_updated', 'asset.version_created' => $this->refresh($media),
        // Withdrawn from public surfaces upstream. Unpublishing here matches
        // that, and is what stops the site rendering something the DAM has
        // pulled — the signed URL will refuse anyway, so this is about not
        // showing a broken image rather than about access.
        'asset.unpublished' => $this->setPublished($media, FALSE),
        'asset.published' => $this->setPublished($media, TRUE),
        // Gated on configuration: a remote system should not remove content
        // from a site that has not asked it to, so the default is off.
        //
        // Never refreshed through the API either way. The asset is gone by
        // definition, so asking would answer 404 — and a refresh that requires
        // an answer would leave this item queued forever. The cached metadata
        // stands as the last thing known about it, which is what an editor
        // needs in order to understand what disappeared.
        'asset.deleted' => $unpublishOnDelete && $this->setPublished($media, FALSE),
        // Archived, restored, back to active. Nothing about the reference
        // changes and the bytes stay fetchable — §2's invariant is that
        // proxies never tier — so this is a metadata refresh and no more.
        'asset.status_changed' => $this->refresh($media),
        default => $this->unknown($event),
      };
      if ($changed) {
        $touched++;
      }
    }

    return $touched;
  }

  /**
   * Every media item referencing this asset.
   *
   * @return \Drupal\media\MediaInterface[]
   *   The referencing media items.
   */
  private function referencing(string $assetId): array {
    $storage = $this->entityTypeManager->getStorage('media');
    $types = $this->entityTypeManager->getStorage('media_type')->loadMultiple();

    $out = [];
    foreach ($types as $type) {
      if ($type->getSource()->getPluginId() !== 'damrs_asset') {
        continue;
      }
      $field = $type->getSource()->getConfiguration()['source_field'] ?? NULL;
      if ($field === NULL) {
        continue;
      }
      // Per type, because the source field's name is per type. One query across
      // all of them is not available: they are different columns.
      $ids = $storage->getQuery()
        ->accessCheck(FALSE)
        ->condition('bundle', $type->id())
        ->condition($field, $assetId)
        ->execute();
      foreach ($storage->loadMultiple($ids) as $media) {
        $out[] = $media;
      }
    }

    return $out;
  }

  /**
   * Clears the mapped fields and saves, so Drupal re-reads the metadata.
   *
   * Only after damrs has answered. See the class docs: clearing first and
   * hoping is how a refresh during an outage becomes data loss.
   */
  private function refresh(MediaInterface $media): bool {
    $asset_id = (string) $this->assetIdOf($media);
    $result = $this->client->fetchAsset($asset_id);
    if ($result->unreachable()) {
      throw new UnreachableException(sprintf(
        'damrs did not answer for %s; leaving the cached metadata alone',
        $asset_id,
      ));
    }
    if (!$result->ok()) {
      // Answered, and the asset is not available to us — deleted, or outside
      // this credential's scope. Keeping the last known metadata beats blanking
      // it: an editor looking at a media item whose asset has gone is better
      // served by a title than by an empty row.
      $this->logger->notice('damrs answered @status for @asset; keeping the cached metadata', [
        '@status' => $result->status ?? 'nothing',
        '@asset' => $asset_id,
      ]);

      return FALSE;
    }

    $type = $media->bundle->entity;
    $map = $type?->getFieldMap() ?? [];
    foreach ($map as $field) {
      if ($media->hasField($field)) {
        $media->set($field, NULL);
      }
    }
    // The thumbnail too: a new version is new bytes, and a cached thumbnail
    // keyed by the old version would otherwise stand until something else
    // cleared it.
    if ($media->hasField('thumbnail')) {
      $media->set('thumbnail', NULL);
    }
    $media->save();

    return TRUE;
  }

  /**
   * The asset id a media item references.
   */
  private function assetIdOf(MediaInterface $media): ?string {
    $field = $media->bundle->entity?->getSource()->getConfiguration()['source_field'] ?? NULL;
    if ($field === NULL || !$media->hasField($field) || $media->get($field)->isEmpty()) {
      return NULL;
    }

    return (string) $media->get($field)->value;
  }

  /**
   * Publishes or unpublishes, saving only if it actually changes.
   */
  private function setPublished(MediaInterface $media, bool $published): bool {
    if ($media->isPublished() === $published) {
      // No save. A no-op save would bump `changed`, invalidate caches and emit
      // an entity update hook for nothing — and on a bulk event that is a lot
      // of nothing.
      return FALSE;
    }
    $published ? $media->setPublished() : $media->setUnpublished();
    $media->save();

    return TRUE;
  }

  /**
   * Logs an event this version does not handle.
   */
  private function unknown(string $event): bool {
    // Not an error. damrs may add events, and an older connector seeing one is
    // expected rather than broken — the queue item is still consumed, because
    // leaving it to retry forever would be worse than ignoring it.
    $this->logger->info('ignoring damrs event @event, which this version does not handle', [
      '@event' => $event,
    ]);

    return FALSE;
  }

}
