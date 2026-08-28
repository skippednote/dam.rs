<?php

declare(strict_types=1);

namespace Drupal\Tests\damrs_search_api\Kernel;

use Drupal\Core\DependencyInjection\ContainerBuilder;
use Drupal\KernelTests\KernelTestBase;
use Drupal\search_api\Entity\Index;
use Drupal\search_api\Entity\Server;
use Drupal\search_api\SearchApiException;
use GuzzleHttp\Client as GuzzleClient;
use GuzzleHttp\Exception\ConnectException;
use GuzzleHttp\Handler\MockHandler;
use GuzzleHttp\HandlerStack;
use GuzzleHttp\Psr7\Request;
use GuzzleHttp\Psr7\Response;
use PHPUnit\Framework\Attributes\Group;

/**
 * Querying damrs through Search API.
 *
 * The interesting assertions are the refusals. A backend that indexes nothing
 * has to say so honestly to Search API's tracker, must not read "clear the
 * index" as permission to touch the library, and has to refuse paging damrs
 * will not serve rather than returning an empty page that reads as "no more
 * results".
 */
#[Group('damrs')]
final class DamrsBackendTest extends KernelTestBase {

  /**
   * {@inheritdoc}
   */
  protected static $modules = [
    'system',
    'user',
    'field',
    'search_api',
    'damrs',
    'damrs_search_api',
  ];

  /**
   * Queued HTTP responses damrs is pretending to give.
   */
  private MockHandler $handler;

  /**
   * {@inheritdoc}
   */
  public function register(ContainerBuilder $container): void {
    parent::register($container);
    $this->handler ??= new MockHandler();
    $container->set('http_client', new GuzzleClient([
      'handler' => HandlerStack::create($this->handler),
    ]));
  }

  /**
   * {@inheritdoc}
   */
  protected function setUp(): void {
    parent::setUp();
    $this->installEntitySchema('user');
    $this->installEntitySchema('search_api_task');
    $this->installSchema('search_api', ['search_api_item']);
    $this->installConfig(['search_api']);

    $this->config('damrs.settings')
      ->set('base_url', 'https://dam.example.test')
      ->set('api_key', 'test-key')
      ->save();

    Server::create([
      'id' => 'damrs',
      'name' => 'damrs',
      'backend' => 'damrs',
      'backend_config' => [],
    ])->save();
    Index::create([
      'id' => 'assets',
      'name' => 'Assets',
      'server' => 'damrs',
      'datasource_settings' => [],
    ])->save();
  }

  /**
   * The backend under test.
   */
  private function backend(): object {
    return Server::load('damrs')->getBackend();
  }

  /**
   * A browse response with one asset and one facet.
   */
  private function browse(int $total = 1): string {
    return json_encode([
      'results' => [
        'items' => [
          [
            'id' => '66666666-7777-8888-9999-aaaaaaaaaaaa',
            'filename' => 'harbour.jpg',
            'mime' => 'image/jpeg',
            'width' => 4000,
          ],
        ],
        'total' => $total,
        'offset' => 0,
        'ranked' => TRUE,
      ],
      'facets' => [
        ['key' => 'brand', 'buckets' => [['value' => 'acme', 'count' => 12]], 'truncated' => FALSE],
      ],
    ]);
  }

  /**
   * A query returns damrs's results and its total.
   */
  public function testQueryReturnsResultsAndTotal(): void {
    $this->handler->append(new Response(200, [], $this->browse(41)));

    $query = Index::load('assets')->query();
    $query->keys('harbour');
    $results = $query->execute();

    self::assertSame(41, $results->getResultCount(), 'the total is damrs\'s, not the page size');
    $items = $results->getResultItems();
    self::assertCount(1, $items);
    $item = reset($items);
    self::assertSame('harbour.jpg', $item->getExtraData('damrs_asset')['filename']);
  }

  /**
   * The facet rail arrives with the results, from the same call.
   */
  public function testFacetsArriveWithTheResults(): void {
    $this->handler->append(new Response(200, [], $this->browse()));

    $results = Index::load('assets')->query()->execute();

    $facets = $results->getExtraData('search_api_facets');
    self::assertArrayHasKey('brand', $facets);
    self::assertSame(12, $facets['brand'][0]['count']);
    self::assertCount(0, $this->handler, 'one call, not one per facet block');
  }

  /**
   * Indexing reports that nothing was stored.
   *
   * Returning the ids would tell Search API's tracker these items are indexed
   * and searchable here. They are not, and the lie surfaces later as results
   * that never appear.
   */
  public function testIndexingStoresNothingAndSaysSo(): void {
    $index = Index::load('assets');

    self::assertSame([], $this->backend()->indexItems($index, []));
    self::assertCount(0, $this->handler, 'and it does not talk to damrs about it');
  }

  /**
   * Clearing the index must not touch the library.
   *
   * A Drupal site clearing its search index has not asked to empty somebody's
   * asset library. A backend that read this as permission to would turn an
   * ordinary administrative action into a catastrophe, so the assertion is that
   * damrs is not contacted at all.
   */
  public function testClearingTheIndexDoesNotTouchDamrs(): void {
    $index = Index::load('assets');

    $this->backend()->deleteItems($index, ['damrs/one', 'damrs/two']);
    $this->backend()->deleteAllIndexItems($index);

    self::assertCount(0, $this->handler);
  }

  /**
   * Paging past what damrs will serve is refused, not silently empty.
   */
  public function testPagingPastTheCapIsRefused(): void {
    $query = Index::load('assets')->query();
    $query->range(5000, 10);

    $this->expectException(SearchApiException::class);
    $query->execute();
  }

  /**
   * A page ending at the cap is trimmed rather than refused.
   */
  public function testPageEndingAtTheCapIsTrimmed(): void {
    $this->handler->append(new Response(200, [], $this->browse()));

    $query = Index::load('assets')->query();
    $query->range(990, 50);
    $query->execute();

    $request = $this->handler->getLastRequest();
    parse_str((string) $request->getUri()->getQuery(), $params);
    self::assertSame('990', $params['offset']);
    self::assertSame('10', $params['limit'], 'trimmed to what damrs will serve');
  }

  /**
   * An unreachable damrs empties the results and warns, rather than failing.
   *
   * A search box that returns nothing is recoverable. A page that throws is
   * not, and this backend can be behind a block on every page of a site.
   */
  public function testUnreachableDamrsWarnsRatherThanThrowing(): void {
    $this->handler->append(new ConnectException(
      'connection refused',
      new Request('GET', 'https://dam.example.test/browse'),
    ));

    $results = Index::load('assets')->query()->execute();

    self::assertSame(0, $results->getResultCount());
    self::assertNotEmpty($results->getWarnings());
  }

}
