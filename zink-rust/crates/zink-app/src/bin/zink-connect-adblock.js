(() => {
  if (window.__zinkAdBlockInstalled) return;
  window.__zinkAdBlockInstalled = true;
  let enabled = true;
  const blockedHosts = window.__zinkBlockedAdHosts || [];
  const selectors = [
    '.ytp-ad-player-overlay','.ytp-ad-text','.ytp-ad-preview-container','.ytp-ad-message-container',
    '.ytp-ad-simple-ad-badge','.ytp-ad-player-overlay-layout','.ytp-ad-visit-advertiser-button','.video-ads',
    '[id^="google_ads"]','[id*="google_ads"]','[class~="advertisement"]','[id^="ad-container"]',
    '[class*=" ad-container"]','[class^="ad-container"]','[data-ad-container]','[data-ad-slot]',
    '[aria-label*="advertisement" i]','[aria-label*="sponsored" i]',
    'iframe[src*="doubleclick"]','iframe[src*="googlesyndication"]','iframe[src*="adservice"]'
  ];
  const skipSelectors = [
    '.ytp-ad-skip-button-modern','.ytp-ad-skip-button','.ytp-skip-ad-button',
    '.ytp-skip-ad-button-modern','button[class*="ad-skip" i]','button[id*="ad-skip" i]',
    'button[aria-label*="skip ad" i]','button[title*="skip ad" i]'
  ];
  const youtubeAdKeys = new Set([
    'ad3_module','adBreakHeartbeatParams','adPlacements','adSlots','playerAds',
    'adPlacementRenderer','instreamAdPlayerOverlayRenderer','linearAdSequenceRenderer'
  ]);

  function isYouTube() {
    return location.hostname === 'youtube.com' || location.hostname.endsWith('.youtube.com');
  }

  function stripYouTubeAds(value, depth = 0) {
    if (!enabled || !isYouTube() || !value || typeof value !== 'object' || depth > 12) return value;
    if (Array.isArray(value)) {
      for (const item of value) stripYouTubeAds(item, depth + 1);
      return value;
    }
    for (const key of Object.keys(value)) {
      if (youtubeAdKeys.has(key)) delete value[key];
      else stripYouTubeAds(value[key], depth + 1);
    }
    return value;
  }

  // YouTube embeds its first advert schedule in the initial player response.
  // Remove it before the player reads it, avoiding the blocked-ad timeout.
  let initialPlayerResponse;
  try {
    Object.defineProperty(window, 'ytInitialPlayerResponse', {
      configurable: true,
      get: () => initialPlayerResponse,
      set: value => { initialPlayerResponse = stripYouTubeAds(value); }
    });
  } catch (_) {}
  const nativeJsonParse = JSON.parse.bind(JSON);
  JSON.parse = function(...args) {
    return stripYouTubeAds(nativeJsonParse(...args));
  };

  function isBlockedUrl(value) {
    if (!enabled || !value) return false;
    try {
      const host = new URL(String(value), location.href).hostname.toLowerCase();
      return blockedHosts.some(rule => host === rule || host.endsWith(`.${rule}`));
    } catch (_) { return false; }
  }

  function hide(node) {
    if (!node || node.dataset.zinkAdHidden) return;
    node.dataset.zinkAdHidden = '1';
    node.dataset.zinkAdStyle = node.getAttribute('style') || '';
    node.style.setProperty('display', 'none', 'important');
    node.style.setProperty('visibility', 'hidden', 'important');
  }

  function restore() {
    for (const node of document.querySelectorAll('[data-zink-ad-hidden="1"]')) {
      const previous = node.dataset.zinkAdStyle || '';
      previous ? node.setAttribute('style', previous) : node.removeAttribute('style');
      delete node.dataset.zinkAdHidden;
      delete node.dataset.zinkAdStyle;
    }
  }

  function cleanPage() {
    if (!enabled) return;
    try {
      for (const selector of skipSelectors) {
        for (const node of document.querySelectorAll(selector)) {
          const button = node.closest('button') || node;
          if (button.getBoundingClientRect().width > 0) button.click();
        }
      }
      for (const selector of selectors) {
        for (const node of document.querySelectorAll(selector)) hide(node);
      }
      for (const frame of document.querySelectorAll('iframe[src]')) {
        if (isBlockedUrl(frame.src)) hide(frame);
      }
    } catch (_) {}
  }

  const nativeOpen = window.open.bind(window);
  window.open = function(url, ...args) {
    if (isBlockedUrl(url)) return null;
    return nativeOpen(url, ...args);
  };
  const nativeFetch = window.fetch ? window.fetch.bind(window) : null;
  if (nativeFetch) window.fetch = async function(input, init) {
    const url = typeof input === 'string' ? input : input && input.url;
    if (isBlockedUrl(url)) return Promise.resolve(new Response('', {status: 204}));
    const response = await nativeFetch(input, init);
    if (enabled && isYouTube() && String(url || '').includes('/youtubei/v1/player')) {
      const nativeJson = response.json.bind(response);
      response.json = async () => stripYouTubeAds(await nativeJson());
    }
    return response;
  };
  const nativeBeacon = navigator.sendBeacon ? navigator.sendBeacon.bind(navigator) : null;
  if (nativeBeacon) navigator.sendBeacon = function(url, data) {
    return isBlockedUrl(url) || nativeBeacon(url, data);
  };

  window.__zinkSetAdBlock = value => {
    enabled = !!value;
    if (enabled) cleanPage(); else restore();
  };
  new MutationObserver(cleanPage).observe(document.documentElement, {childList:true,subtree:true});
  setInterval(cleanPage, 500);
  cleanPage();
})();
