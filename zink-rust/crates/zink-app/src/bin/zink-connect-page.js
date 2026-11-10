(() => {
  if (window.__zinkConnectProtectionInstalled) return;
  window.__zinkConnectProtectionInstalled = true;

  function reportFullscreen() {
    try {
      const youtubePlayer = document.querySelector('.html5-video-player');
      const active = !!(
        document.fullscreenElement ||
        document.webkitFullscreenElement ||
        (youtubePlayer && youtubePlayer.classList.contains('ytp-fullscreen'))
      );
      window.ipc.postMessage(active ? 'fullscreen:enter' : 'fullscreen:exit');
    } catch (_) {}
  }

  // Match the original WinUI WebView2 ContainsFullScreenElementChanged path for
  // every standards-based HTML5 player, with YouTube's CSS player state as the
  // same fallback used by the previous app.
  document.addEventListener('fullscreenchange', reportFullscreen, true);
  document.addEventListener('webkitfullscreenchange', reportFullscreen, true);
  document.addEventListener('click', (event) => {
    try {
      const control = event.target && event.target.closest
        ? event.target.closest(
            'button[aria-label*="full screen" i],button[aria-label*="fullscreen" i],' +
            'button[title*="full screen" i],button[title*="fullscreen" i],' +
            '.ytp-fullscreen-button,[data-fullscreen]'
          )
        : null;
      if (control) {
        setTimeout(reportFullscreen, 80);
        setTimeout(reportFullscreen, 250);
      }
    } catch (_) {}
  }, true);

  const qualityRank = ['highres','hd2160','hd1440','hd1080','hd720','large','medium','small','tiny','auto'];
  function forceHighestQuality() {
    try {
      if (!location.hostname.includes('youtube.com')) return;
      const player = document.getElementById('movie_player') || document.querySelector('.html5-video-player');
      if (!player || typeof player.getAvailableQualityLevels !== 'function') return;
      const levels = player.getAvailableQualityLevels();
      const best = qualityRank.find(level => levels.includes(level));
      if (!best) return;
      if (typeof player.setPlaybackQualityRange === 'function') player.setPlaybackQualityRange(best, best);
      if (typeof player.setPlaybackQuality === 'function') player.setPlaybackQuality(best);
    } catch (_) {}
  }

  document.addEventListener('yt-navigate-finish', () => { for (let i=0;i<20;i++) setTimeout(forceHighestQuality,i*500); }, true);
  setInterval(forceHighestQuality, 5000);

  // Preserve the Xbox page's achievement observer when Xbox Cloud Gaming is
  // hosted in the shared Zink Connect WebView rather than a page-owned view.
  const seenAchievements = new Map();
  const achievementPattern = /achievement\s+(unlocked|earned)|unlocked\s+achievement/i;
  const scorePattern = /\b(\d{1,5})\s*G\b/i;
  function cleanAchievementText(value) {
    return (value || '').replace(/\s+/g, ' ').trim();
  }
  function inspectAchievement(node) {
    try {
      if (!location.hostname.includes('xbox.com') || !(node instanceof Element)) return;
      const text = cleanAchievementText(node.innerText || node.textContent);
      if (!text || text.length > 700 || !achievementPattern.test(text)) return;
      const key = text.slice(0, 250);
      const now = Date.now();
      if (seenAchievements.has(key) && now - seenAchievements.get(key) < 12000) return;
      seenAchievements.set(key, now);
      const lines = (node.innerText || text).split(/\r?\n/).map(cleanAchievementText).filter(Boolean);
      const scoreMatch = text.match(scorePattern);
      const title = lines.find(line =>
        !achievementPattern.test(line) && !scorePattern.test(line) && line.length > 2 && line.length < 100
      ) || 'Xbox achievement';
      window.ipc.postMessage('xbox-achievement:' + JSON.stringify({
        title,
        score: scoreMatch ? `${scoreMatch[1]} G` : 'G',
        description: 'Xbox achievement added to your profile'
      }));
    } catch (_) {}
  }
  const achievementObserver = new MutationObserver(records => {
    for (const record of records) for (const node of record.addedNodes) inspectAchievement(node);
  });
  if (document.documentElement) {
    achievementObserver.observe(document.documentElement, { childList: true, subtree: true });
  }
})();
