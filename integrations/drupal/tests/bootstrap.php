<?php

/**
 * @file
 * Autoloading for the unit tests, with no Drupal and no composer install.
 *
 * The signer deliberately depends on nothing from Drupal — that is what lets it
 * run in the render path without a container — so its tests need nothing from
 * Drupal either. Installing `drupal/core` to assert a byte comparison would
 * make the one job that guards the wire format the slowest job in CI, and a
 * slow guard is a guard somebody eventually marks `continue-on-error`.
 *
 * Classes that *do* need Drupal (the client, the settings form) are covered by
 * Drupal's own test runner inside a site, not from here.
 */

declare(strict_types=1);

spl_autoload_register(static function (string $class): void {
  $prefix = 'Drupal\\damrs\\';
  if (!str_starts_with($class, $prefix)) {
    return;
  }
  $relative = str_replace('\\', '/', substr($class, strlen($prefix)));
  $path = __DIR__ . '/../src/' . $relative . '.php';
  if (is_file($path)) {
    require $path;
  }
});
