(() => {
  const ROOT_ID = "zink-connect-chrome";
  const STYLE_ID = "zink-connect-page-style";

  function readFlag(name, fallback) {
    try {
      const value = localStorage.getItem(name);
      return value === null ? fallback : value === "true";
    } catch (_) {
      return fallback;
    }
  }

  function writeFlag(name, value) {
    try { localStorage.setItem(name, String(value)); } catch (_) {}
  }

  function normaliseAddress(value) {
    const text = value.trim();
    if (!text) return "https://www.bing.com";
    if (/^[a-z][a-z0-9+.-]*:\/\//i.test(text)) return text;
    if (/^[^\s]+\.[^\s]+$/.test(text)) return `https://${text}`;
    return `https://www.bing.com/search?q=${encodeURIComponent(text)}`;
  }

  function installPageFiltering(blockAds, blockVideoAds) {
    let style = document.getElementById(STYLE_ID);
    if (!style) {
      style = document.createElement("style");
      style.id = STYLE_ID;
      (document.head || document.documentElement).appendChild(style);
    }
    const rules = [];
    if (blockAds) {
      rules.push('[id*="ad-"]', '[id^="ad_"]', '[class*=" ad-"]',
        '[class^="ad-"]', '[class*="advert"]', '[aria-label*="advert" i]',
        'iframe[src*="doubleclick"]', 'iframe[src*="googlesyndication"]');
    }
    if (blockVideoAds) {
      rules.push('.video-ads', '.ytp-ad-module', '.ytp-ad-overlay-container',
        '[class*="preroll"]', '[class*="midroll"]');
    }
    style.textContent = rules.length ? `${rules.join(",")} { display:none !important; }` : "";
  }

  function installChrome() {
    if (!document.documentElement || document.getElementById(ROOT_ID)) return;

    const host = document.createElement("div");
    host.id = ROOT_ID;
    host.style.cssText = "position:fixed;inset:0 0 auto 0;height:102px;z-index:2147483647;pointer-events:auto";
    host.innerHTML = `
      <style>
        #zink-connect-chrome,#zink-connect-chrome *{pointer-events:auto!important}
        #zink-connect-chrome *{box-sizing:border-box} #zink-connect-chrome .shell{height:102px;padding:8px 12px 10px;color:#f5ffff;
        font:13px 'Segoe UI',sans-serif;background:linear-gradient(135deg,#071018f7,#101d28f4);
        border-bottom:1px solid #36ffffff;box-shadow:0 8px 26px #0008}
        #zink-connect-chrome .tabs,#zink-connect-chrome .bar{display:flex;align-items:center;gap:7px}#zink-connect-chrome .tabs{height:37px;margin-bottom:7px}
        #zink-connect-chrome .tab{height:32px;min-width:190px;max-width:280px;padding:0 12px;border-radius:16px;
        display:flex;align-items:center;gap:9px;background:#5267f7f3;border:1px solid #8467f7f3}
        #zink-connect-chrome .title{flex:1;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}#zink-connect-chrome .x,#zink-connect-chrome .icon,#zink-connect-chrome .go,#zink-connect-chrome .newtab{border:1px solid #36ffffff;
        background:#22ffffff;color:white;border-radius:16px;height:34px;cursor:pointer}#zink-connect-chrome .x{border:0;background:transparent;height:auto}
        #zink-connect-chrome .newtab{width:34px;font-size:18px}#zink-connect-chrome .icon{width:38px;font-size:17px}#zink-connect-chrome .icon:hover,#zink-connect-chrome .newtab:hover{background:#32ffffff}
        #zink-connect-chrome .brand{width:38px;height:38px;border-radius:14px;display:grid;place-items:center;color:#67f7f3;
        background:#2467f7f3;border:1px solid #5867f7f3;font-size:19px}#zink-connect-chrome .address{height:38px;min-width:180px;flex:1;
        border:1px solid #50ffffff;border-radius:17px;padding:0 14px;color:#101820;background:#ecf7fbff;outline:none}
        #zink-connect-chrome label{display:flex;align-items:center;gap:5px;color:#bbd5e4ee;font-size:12px;white-space:nowrap}
        #zink-connect-chrome .go{padding:0 18px;background:#3067f7f3;border-color:#7067f7f3}#zink-connect-chrome .loading{position:absolute;left:0;right:0;bottom:0;height:2px;
        background:linear-gradient(90deg,transparent,#67f7f3,transparent);animation:load 1s linear infinite;display:none}
        @keyframes load{from{transform:translateX(-70%)}to{transform:translateX(70%)}}
      </style>
      <div class="shell">
        <div class="tabs"><button class="newtab" title="New tab">+</button><div class="tab"><span>◎</span><span class="title">New tab</span><button class="x" title="Close tab">×</button></div><span style="color:#67f7f3;margin-left:auto">Zink Connect 5.0</span></div>
        <div class="bar"><span class="brand">◎</span><button class="icon back" title="Back">‹</button><button class="icon forward" title="Forward">›</button>
        <button class="icon refresh" title="Refresh">↻</button><input class="address" aria-label="Search or enter a website" />
        <label>Ads <input class="ads" type="checkbox" /></label><label>Video ads <input class="video-ads" type="checkbox" /></label>
        <button class="go">Go</button></div><div class="loading"></div>
      </div>`;
    document.documentElement.appendChild(host);
    document.documentElement.style.setProperty("scroll-padding-top", "108px", "important");
    if (document.body) document.body.style.setProperty("padding-top", "102px", "important");

    host.addEventListener("pointerdown", (event) => event.stopPropagation());
    host.addEventListener("click", (event) => event.stopPropagation());
    const $ = (selector) => host.querySelector(selector);
    const address = $(".address");
    const title = $(".title");
    const ads = $(".ads");
    const videoAds = $(".video-ads");
    address.value = location.href;
    title.textContent = document.title || new URL(location.href).hostname || "New tab";
    ads.checked = readFlag("zink-block-ads", true);
    videoAds.checked = readFlag("zink-block-video-ads", true);
    installPageFiltering(ads.checked, videoAds.checked);

    $(".back").onclick = () => history.back();
    $(".forward").onclick = () => history.forward();
    $(".refresh").onclick = () => location.reload();
    const navigate = () => { $(".loading").style.display = "block"; location.href = normaliseAddress(address.value); };
    $(".go").onclick = navigate;
    address.onkeydown = (event) => { if (event.key === "Enter") navigate(); };
    $(".newtab").onclick = () => { location.href = "https://www.bing.com"; };
    $(".x").onclick = () => { location.href = "https://www.bing.com"; };
    ads.onchange = () => { writeFlag("zink-block-ads", ads.checked); installPageFiltering(ads.checked, videoAds.checked); };
    videoAds.onchange = () => { writeFlag("zink-block-video-ads", videoAds.checked); installPageFiltering(ads.checked, videoAds.checked); };

    new MutationObserver(() => {
      title.textContent = document.title || title.textContent;
      installPageFiltering(ads.checked, videoAds.checked);
    }).observe(document.documentElement, { childList: true, subtree: true });
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", installChrome, { once: true });
  } else {
    installChrome();
  }
})();
