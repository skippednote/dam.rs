<?php

declare(strict_types=1);

namespace Drupal\damrs_search_api\Plugin\search_api\backend;

use Drupal\Core\StringTranslation\TranslatableMarkup;
use Drupal\damrs\Client;
use Drupal\search_api\Attribute\SearchApiBackend;
use Drupal\search_api\Backend\BackendPluginBase;
use Drupal\search_api\IndexInterface;
use Drupal\search_api\Query\QueryInterface;
use Drupal\search_api\SearchApiException;
use Symfony\Component\DependencyInjection\ContainerInterface;

/**
 * A Search API backend that asks damrs instead of a local index.
 *
 * ## It does not index, and that is the design rather than a shortcut.
 *
 * Search API's usual shape is index-then-query: content is copied into a
 * backend, and searches run against the copy. Doing that here would put a
 * second copy of somebody's library inside Drupal and make every rights
 * decision be taken against it — so an asset whose licence lapsed, or whose
 * access changed, would keep appearing in results until the next reindex. The
 * whole argument for this connector is that rights stay authoritative in the
 * DAM, and an indexed copy quietly undoes it.
 *
 * So `indexItems()` accepts nothing and `search()` proxies. damrs answers with
 * what *this connector's credential* may see, evaluated at query time, which is
 * the same guarantee the delivery path gives for bytes.
 *
 * ## What that costs, stated plainly.
 *
 * Every search is an HTTP round trip. One, though, not several: `/browse`
 * answers with the results and the facet rail together, counted over the same
 * query — which is also what stops a facet claiming forty while the grid beside
 * it shows three. There is no offline search: if damrs is unreachable the
 * results are empty and a warning is attached, rather than the page failing. A
 * search box that returns nothing is recoverable; a white screen is not.
 *
 * And paging is bounded. damrs caps search depth deliberately, because deep
 * paging over a ranked query means sorting the whole library by a score nobody
 * will read; a request past that cap is refused here with a plain message
 * instead of silently returning an empty page that looks like "no more
 * results".
 */
#[SearchApiBackend(
  id: 'damrs',
  label: new TranslatableMarkup('damrs'),
  description: new TranslatableMarkup('Queries a damrs library directly. Nothing is indexed in Drupal, so rights are evaluated by damrs at query time.'),
)]
final class DamrsBackend extends BackendPluginBase {

  /**
   * How deep damrs will page a ranked query.
   *
   * Mirrors `MAX_SEARCH_DEPTH` in `dam_api::search`. Duplicated rather than
   * fetched because there is no endpoint that reports it, and a Drupal view
   * asking for page 500 should be told so rather than shown an empty grid.
   */
  private const MAX_DEPTH = 1000;

  public function __construct(
    array $configuration,
    $plugin_id,
    $plugin_definition,
    private readonly Client $client,
  ) {
    parent::__construct($configuration, $plugin_id, $plugin_definition);
  }

  /**
   * {@inheritdoc}
   */
  public static function create(ContainerInterface $container, array $configuration, $plugin_id, $plugin_definition): static {
    $backend = new static(
      $configuration,
      $plugin_id,
      $plugin_definition,
      $container->get('damrs.client'),
    );
    // Through the setter the base class provides, not a constructor property:
    // `BackendPluginBase` already declares `$fieldsHelper`, and promoting a
    // readonly one of the same name is a fatal error rather than an override.
    $backend->setFieldsHelper($container->get('search_api.fields_helper'));

    return $backend;
  }

  /**
   * {@inheritdoc}
   */
  public function getSupportedFeatures(): array {
    // Facets, because that is what this backend is for: `/browse` returns the
    // rail alongside the results, counted over the same query, so a Drupal
    // facet block reflects the library rather than a copy of it.
    return ['search_api_facets'];
  }

  /**
   * {@inheritdoc}
   */
  public function viewSettings(): array {
    return [
      [
        'label' => $this->t('Indexing'),
        'info' => $this->t('None. Queries are answered by damrs at request time, so rights and access are evaluated there rather than against a copy held here.'),
      ],
      [
        'label' => $this->t('Maximum paging depth'),
        'info' => $this->t('@depth results. damrs refuses deeper paging on a ranked query.', ['@depth' => self::MAX_DEPTH]),
      ],
    ];
  }

  /**
   * {@inheritdoc}
   */
  public function indexItems(IndexInterface $index, array $items): array {
    // Nothing is stored, so nothing succeeded. Returning the ids would tell
    // Search API's tracker that these items are indexed and searchable here,
    // which would be a lie that shows up later as results that never appear.
    if ($items !== []) {
      $this->getLogger()->info('damrs indexes nothing locally; @count item(s) were not stored', [
        '@count' => count($items),
      ]);
    }

    return [];
  }

  /**
   * {@inheritdoc}
   */
  public function deleteItems(IndexInterface $index, array $item_ids): void {
    // Nothing was stored, so there is nothing to delete. Silent rather than
    // logged: Search API calls this routinely as content changes, and a log
    // line per deletion would be noise about a no-op.
  }

  /**
   * {@inheritdoc}
   */
  public function deleteAllIndexItems(IndexInterface $index, $datasource_id = NULL): void {
    // As above. Notably this must *not* try to clear anything in damrs: a
    // Drupal site clearing its search index has not asked to empty somebody's
    // asset library, and a backend that read this as permission to would be a
    // catastrophe triggered by an ordinary administrative action.
  }

  /**
   * {@inheritdoc}
   */
  public function search(QueryInterface $query): void {
    $results = $query->getResults();
    $index = $query->getIndex();

    $keys = $query->getKeys();
    $q = is_array($keys) ? $this->flatten($keys) : (string) ($keys ?? '');

    [$offset, $limit] = $this->range($query);
    if ($offset >= self::MAX_DEPTH) {
      // Refused, not empty. An empty page here is indistinguishable from "no
      // more results", and a view would show a blank grid with paging that
      // appears to work.
      throw new SearchApiException(sprintf(
        'damrs does not page beyond %d results on a ranked query; narrow the search instead.',
        self::MAX_DEPTH,
      ));
    }
    $limit = min($limit, self::MAX_DEPTH - $offset);

    $response = $this->client->browse([
      'q' => $q,
      'offset' => $offset,
      'limit' => $limit,
    ]);

    if ($response === []) {
      // Unreachable, or refused. An empty result set with a warning leaves the
      // page working and tells the person why it is empty; throwing would take
      // down every page carrying a search block.
      $results->setResultCount(0);
      $results->addWarning($this->t('damrs did not answer, so no results could be loaded.'));

      return;
    }

    $page = $response['results'] ?? [];
    $items = $page['items'] ?? [];
    $results->setResultCount((int) ($page['total'] ?? count($items)));

    foreach ($items as $item) {
      if (!isset($item['id'])) {
        continue;
      }
      $id = 'damrs/' . $item['id'];
      $result = $this->getFieldsHelper()->createItem($index, $id);
      // The summary damrs already returned, so a result list can render without
      // a second call per row. Search API stores this as extra data rather than
      // as indexed fields, which is honest: these are not fields this backend
      // holds, they are what the query happened to come back with.
      $result->setExtraData('damrs_asset', $item);
      $results->addResultItem($result);
    }

    // The rail came back with the results, so there is nothing further to ask
    // for. Attached whether or not this query declared facets: it cost nothing
    // to receive, and a facet block rendering later in the request would
    // otherwise have no data.
    $this->attachFacets($query, $response['facets'] ?? []);
  }

  /**
   * Translates damrs's facet rail into the shape Search API expects.
   *
   * @param \Drupal\search_api\Query\QueryInterface $query
   *   The query whose results carry the rail.
   * @param array $facets
   *   The rail as damrs returned it.
   */
  private function attachFacets(QueryInterface $query, array $facets): void {
    if ($facets === []) {
      return;
    }

    $out = [];
    foreach ($facets as $facet) {
      $key = (string) ($facet['key'] ?? '');
      if ($key === '') {
        continue;
      }
      foreach ($facet['buckets'] ?? [] as $bucket) {
        $out[$key][] = [
          // Search API's own shape: a filter expression and a count.
          'filter' => '"' . (string) ($bucket['value'] ?? '') . '"',
          'count' => (int) ($bucket['count'] ?? 0),
        ];
      }
    }

    $query->getResults()->setExtraData('search_api_facets', $out);
  }

  /**
   * The offset and limit a query asks for.
   *
   * @return array
   *   The offset and the limit, in that order.
   */
  private function range(QueryInterface $query): array {
    $options = $query->getOptions();

    return [
      (int) ($options['offset'] ?? 0),
      max(1, (int) ($options['limit'] ?? 50)),
    ];
  }

  /**
   * Flattens Search API's nested key structure into damrs's query shorthand.
   *
   * Only the terms survive. Search API expresses conjunction and negation in
   * the structure, and damrs has its own shorthand for both — mapping between
   * them is guesswork that would silently change what a person searched for, so
   * this passes the words and lets damrs parse them. A caller wanting damrs's
   * operators types them.
   */
  private function flatten(array $keys): string {
    $terms = [];
    foreach ($keys as $key => $value) {
      if ($key === '#conjunction' || $key === '#negation') {
        continue;
      }
      $terms[] = is_array($value) ? $this->flatten($value) : (string) $value;
    }

    return trim(implode(' ', array_filter($terms, static fn (string $t): bool => $t !== '')));
  }

}
