<?php

declare(strict_types=1);

namespace Drupal\damrs_sync;

/**
 * damrs could not be asked, so the event was not applied.
 *
 * Its own type because the queue worker has to treat it differently from every
 * other failure: this one means "try again", and almost everything else means
 * "this item will never work". Catching a generic exception and retrying would
 * turn a malformed item into an entry that never drains.
 */
final class UnreachableException extends \RuntimeException {}
