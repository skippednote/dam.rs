# damrs for Drupal

Connects a Drupal 11 site to a damrs library. Assets stay in the DAM; Drupal stores a reference.

**Status: `damrs` and `damrs_media`.** The API client, settings, service-account auth, the delivery-URL
signer and the `damrs_asset` media source are here, verified against a live Drupal 11.4 and against damrs
itself. Four submodules remain — `damrs_image_style`, `damrs_sync`, `damrs_editor`, `damrs_search_api`. See
`TASKS.md` (M3d·5) for what each is for.

## Why reference and not copy

The media entity stores an asset id, a version, cached metadata and cached transform URLs. The bytes never
enter `sites/default/files`.

This is not a storage optimisation. It is what makes rights authoritative: when a licence expires in the DAM,
the image stops rendering on the site. If Drupal had copied the file, expiry in the DAM would be cosmetic and
an expired-licence image would sit on a live site indefinitely — which is a legal exposure, and closing it is
the connector's single strongest argument.

## Why rendering never calls damrs

Transform URLs are HMAC-signed **in PHP**, from the shared secret, by `Signing\Signer`. Painting a page
makes no request to damrs, so an outage upstream degrades to stale-but-working pages rather than white
screens or a stalled render queue. A CMS integration that hard-depends on an upstream API to paint a page is
not shippable.

`Client` is the other half — the editorial surfaces, where waiting on an API is expected and a failure has
somewhere sensible to be reported. It returns `NULL` or an empty result and logs rather than throwing: a
media entity rendering a stale cached title is the right outcome of an outage, and an uncaught exception from
a field formatter is not.

A signed URL is permission to *attempt*, never permission to receive. damrs evaluates rights when the URL is
fetched, which is why signing locally is safe — and why revoking a licence takes effect on URLs this module
has already rendered into a cached page.

## Secret rotation

This module signs, so **this module decides when to switch**. During a rotation the same key id is live under
two secrets and damrs accepts either, which is what `previous_signing_secret` is for. Without that window,
rotating would invalidate every URL already rendered into a cached page — a rotation that takes the site
down.

## The wire format, and how it is kept honest

There are now two implementations of the delivery-token canonical form, in two languages, and they have to
agree byte for byte forever. `tests/fixtures/signing_vectors.json` is generated from the Rust:

```sh
cargo run -p dam-core --example signing_vectors > integrations/drupal/tests/fixtures/signing_vectors.json
```

`tests/src/Unit/SignerTest.php` compares against it offline. Change the canonical form and the fixture
changes, the diff is visible in review, and the PHP suite fails until it is updated too.

The vectors deliberately include the cases a reimplementation gets wrong, and each was confirmed to fail the
suite when introduced deliberately:

| Mistake | Caught by |
|---|---|
| Character length instead of byte length | `non_ascii_transform` |
| Omitting an absent optional instead of writing a zero-length field | `minimal`, and three others |
| Signing a UUID's text instead of its 16 raw bytes | every case |
| A `u8` length prefix | `long_transform` |

To check a token produced from real configuration rather than a fixed claim — which catches a signer whose
claim *assembly* is wrong rather than whose encoding is:

```sh
cargo run -p dam-core --example verify_token -- <secret> <key-id> <token>
```

## Development

The module is developed against a throwaway DDEV site rather than inside this repository, since nothing here
runs PHP:

```sh
mkdir drupal-test && cd drupal-test
ddev config --project-type=drupal11 --project-name=damrs-drupal --docroot=web --php-version=8.3
ddev start
ddev composer create-project drupal/recommended-project --no-interaction
ddev composer require drush/drush --no-interaction
ddev drush site:install standard --account-name=admin --account-pass=admin -y
rsync -a --delete /path/to/damrs/integrations/drupal/ web/modules/custom/damrs/
ddev drush en damrs -y
```

Then point it at a library at `/admin/config/media/damrs`.

### Running the tests

Unit tests need neither Drupal nor composer — the signer depends on nothing from Drupal, which is the same
property that lets it run in the render path:

```sh
cd integrations/drupal
curl -sSLo phpunit.phar https://phar.phpunit.de/phpunit-11.phar
php phpunit.phar --configuration phpunit.xml.dist
```

Kernel tests do need a site, because what they pin is how Drupal *calls* the source plugin rather than what
it returns. From the site root, with `drupal/core-dev` installed:

```sh
cp web/modules/custom/damrs/tests/phpunit-kernel.xml.dist phpunit-kernel.xml
./vendor/bin/phpunit -c phpunit-kernel.xml
```

And the coding standards, which §11.2's "contrib-shaped" requires:

```sh
./vendor/bin/phpcs --standard=Drupal,DrupalPractice \
  --extensions=php,module,inc,install,test,info,yml web/modules/custom/damrs
```

CI runs all three.
