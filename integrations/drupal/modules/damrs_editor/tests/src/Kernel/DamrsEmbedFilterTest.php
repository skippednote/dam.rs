<?php

declare(strict_types=1);

namespace Drupal\Tests\damrs_editor\Kernel;

use Drupal\filter\Entity\FilterFormat;
use Drupal\filter\FilterProcessResult;
use Drupal\Core\DependencyInjection\ContainerBuilder;
use Drupal\KernelTests\KernelTestBase;
use GuzzleHttp\Client as GuzzleClient;
use GuzzleHttp\Exception\ConnectException;
use GuzzleHttp\Handler\MockHandler;
use GuzzleHttp\HandlerStack;
use GuzzleHttp\Psr7\Request;
use GuzzleHttp\Psr7\Response;
use PHPUnit\Framework\Attributes\Group;

/**
 * Turning pasted damrs links into embeds.
 *
 * The cases that matter are the ones where the filter should *not* act: a body
 * with no damrs link must not cost an HTTP request, a URL mentioned in prose
 * must survive, and a damrs that will not describe an asset must leave the
 * author's link working rather than replacing it with nothing.
 */
#[Group('damrs')]
final class DamrsEmbedFilterTest extends KernelTestBase {

  /**
   * {@inheritdoc}
   */
  protected static $modules = [
    'system',
    'user',
    'filter',
    'damrs',
    'damrs_editor',
  ];

  private const BASE = 'https://dam.example.test';
  private const ASSET = '66666666-7777-8888-9999-aaaaaaaaaaaa';

  /**
   * Queued HTTP responses damrs is pretending to give.
   */
  private MockHandler $handler;

  /**
   * {@inheritdoc}
   */
  public function register(ContainerBuilder $container): void {
    parent::register($container);
    // Replaced at container-build time, not in setUp(). Setting it afterwards
    // is too late whenever anything has already caused `damrs.client` to be
    // constructed: the client keeps the real Guzzle it was handed, the mock is
    // never consumed, and the test silently exercises a real DNS lookup that
    // fails. That is exactly what happened here — the queued response went
    // untouched and the call returned NULL as if damrs had refused.
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
    $this->installConfig(['filter', 'system']);

    $this->config('damrs.settings')
      ->set('base_url', self::BASE)
      ->set('api_key', 'test-key')
      ->save();

    FilterFormat::create([
      'format' => 'damrs_test',
      'name' => 'damrs test',
      'filters' => ['damrs_embed' => ['status' => TRUE]],
    ])->save();
  }

  /**
   * Runs the filter over some markup.
   */
  private function filter(string $html): FilterProcessResult {
    $format = FilterFormat::load('damrs_test');
    $filter = $format->filters('damrs_embed');

    return $filter->process($html, 'en');
  }

  /**
   * An oEmbed photo response.
   */
  private function photo(int $cacheAge = 900): string {
    return json_encode([
      'type' => 'photo',
      'version' => '1.0',
      'title' => 'A boat at dawn',
      'provider_name' => 'damrs',
      'url' => self::BASE . '/d/signed-token',
      'width' => 1024,
      'height' => 768,
      'cache_age' => $cacheAge,
    ]);
  }

  /**
   * A pasted link becomes an image.
   */
  public function testPastedLinkBecomesAnImage(): void {
    $this->handler->append(new Response(200, [], $this->photo()));

    $result = $this->filter(
      '<p><a href="' . self::BASE . '/assets/' . self::ASSET . '">the boat</a></p>',
    );
    $html = $result->getProcessedText();

    self::assertStringContainsString('<img', $html);
    self::assertStringContainsString(self::BASE . '/d/signed-token', $html);
    self::assertStringContainsString('alt="A boat at dawn"', $html);
    self::assertStringNotContainsString('<a href', $html, 'the link is replaced, not wrapped');
  }

  /**
   * The filtered body must not outlive the URL inside it.
   *
   * The same trap as the formatter, in a second place: this result is cached
   * separately, so a body cached for longer than damrs's reported `cache_age`
   * serves a signed URL that has since expired.
   */
  public function testTheResultDoesNotOutliveTheEmbeddedUrl(): void {
    $this->handler->append(new Response(200, [], $this->photo(900)));

    $result = $this->filter(
      '<p><a href="' . self::BASE . '/assets/' . self::ASSET . '">x</a></p>',
    );

    self::assertSame(900, $result->getCacheMaxAge());
  }

  /**
   * With several embeds, the shortest cache age wins.
   *
   * One expired URL is enough to break the page, so the body can only be kept
   * as long as its most impatient embed allows.
   */
  public function testTheShortestCacheAgeWins(): void {
    $this->handler->append(new Response(200, [], $this->photo(900)));
    $this->handler->append(new Response(200, [], $this->photo(300)));

    $result = $this->filter(
      '<p><a href="' . self::BASE . '/assets/' . self::ASSET . '">a</a></p>'
      . '<p><a href="' . self::BASE . '/assets/' . self::ASSET . '">b</a></p>',
    );

    self::assertSame(300, $result->getCacheMaxAge());
  }

  /**
   * A body with no damrs link costs no request at all.
   *
   * This filter runs on every filtered field on the site, and almost none of
   * them mention damrs. Loading the DOM and walking it for those would be a
   * cost paid on every page for nothing.
   */
  public function testBodyWithNoDamrsLinkCostsNothing(): void {
    $result = $this->filter('<p>Ordinary text with <a href="https://example.com">a link</a>.</p>');

    self::assertStringContainsString('Ordinary text', $result->getProcessedText());
    self::assertCount(0, $this->handler, 'damrs was not asked about anything');
  }

  /**
   * A URL mentioned in prose is left alone.
   *
   * An author writing about the DAM has to be able to quote an asset URL
   * without it turning into a picture. A filter that cannot be escaped is one
   * people route around.
   */
  public function testUrlInProseIsLeftAlone(): void {
    $text = '<p>Paste ' . self::BASE . '/assets/' . self::ASSET . ' to embed it.</p>';

    $result = $this->filter($text);

    self::assertStringContainsString('/assets/' . self::ASSET, $result->getProcessedText());
    self::assertStringNotContainsString('<img', $result->getProcessedText());
    self::assertCount(0, $this->handler);
  }

  /**
   * A link damrs will not describe stays a link.
   *
   * Unreachable, or an asset this site's credential may not see. Either way the
   * author's link still works for whoever does have access, which is better
   * than an empty element or a filter that throws.
   */
  public function testUndescribableLinkIsLeftAlone(): void {
    $this->handler->append(new Response(403, [], ''));

    $html = $this->filter(
      '<p><a href="' . self::BASE . '/assets/' . self::ASSET . '">the boat</a></p>',
    )->getProcessedText();

    self::assertStringContainsString('<a href="' . self::BASE . '/assets/' . self::ASSET . '"', $html);
    self::assertStringNotContainsString('<img', $html);
  }

  /**
   * An unreachable damrs leaves the body untouched rather than failing.
   *
   * A filter that threw would take down every page holding a damrs link for as
   * long as the outage lasted, which is a far larger failure than the embed not
   * appearing.
   */
  public function testUnreachableDamrsLeavesTheBodyIntact(): void {
    $this->handler->append(new ConnectException(
      'connection refused',
      new Request('GET', self::BASE . '/oembed'),
    ));

    $html = $this->filter(
      '<p><a href="' . self::BASE . '/assets/' . self::ASSET . '">the boat</a></p>',
    )->getProcessedText();

    self::assertStringContainsString('the boat', $html);
    self::assertStringNotContainsString('<img', $html);
  }

  /**
   * Anything that is not a photo becomes a thumbnail link, not a broken embed.
   */
  public function testNonPhotoBecomesThumbnailLink(): void {
    $this->handler->append(new Response(200, [], json_encode([
      'type' => 'link',
      'version' => '1.0',
      'title' => 'The quarterly report',
      'provider_name' => 'damrs',
      'url' => self::BASE . '/d/pdf-token',
      'thumbnail_url' => self::BASE . '/d/thumb-token',
      'cache_age' => 900,
    ])));

    $html = $this->filter(
      '<p><a href="' . self::BASE . '/assets/' . self::ASSET . '">report</a></p>',
    )->getProcessedText();

    self::assertStringContainsString('thumb-token', $html);
    self::assertStringContainsString('href="' . self::BASE . '/d/pdf-token"', $html);
  }

}
