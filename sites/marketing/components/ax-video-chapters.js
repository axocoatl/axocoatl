// Shared by marketing walkthroughs and the Session/required-review documentation.
const pendingSeeks = new WeakMap();

function cancelPendingSeek(video) {
  const pending = pendingSeeks.get(video);
  if (pending) video.removeEventListener('loadedmetadata', pending.seek);
  pendingSeeks.delete(video);
}

function seekVideo(video, time, source) {
  // Seeking can temporarily lower readyState; never mistake that for an initial
  // metadata load and reset the position with video.load().
  const needsMetadata = video.readyState < HTMLMediaElement.HAVE_METADATA;
  // A later chapter choice replaces an earlier request while metadata loads.
  cancelPendingSeek(video);
  const seek = () => {
    // Pausing exactly on a shared WebVTT endpoint can display both cues. A URL
    // opens 1ms inside the new chapter; canonical times and button seeks stay exact.
    const targetTime = source === 'url' && time > 0 ? time + 0.001 : time;
    video.currentTime = Number.isFinite(video.duration) ? Math.min(targetTime, video.duration) : targetTime;
    pendingSeeks.delete(video);
  };
  if (!needsMetadata) {
    seek();
  } else {
    pendingSeeks.set(video, { seek, source });
    video.addEventListener('loadedmetadata', seek, { once: true });
  }

  if (source === 'button') {
    // Start within the user gesture, including when media still needs to load.
    // Native controls remain available if the browser declines playback.
    video.play().catch(() => {});
  } else if (needsMetadata) {
    // A cold URL needs metadata to seek, but must not start playback.
    if (video.preload === 'none') video.preload = 'metadata';
    video.load();
  }
  video.scrollIntoView({ block: 'nearest', behavior: 'instant' });
}

document.addEventListener('click', (event) => {
  const button = event.target instanceof Element
    ? event.target.closest('button[data-video-target][data-video-time]') : null;
  if (!button) return;
  const video = document.getElementById(button.dataset.videoTarget);
  const time = Number(button.dataset.videoTime);
  if (!(video instanceof HTMLVideoElement) || !Number.isFinite(time) || time < 0) return;
  seekVideo(video, time, 'button');
});

let previousUrl;
let urlVideo;
function seekUrlChapter() {
  // History navigation can emit both popstate and hashchange for the same URL.
  if (location.href === previousUrl) return;
  previousUrl = location.href;
  if (urlVideo && pendingSeeks.get(urlVideo)?.source === 'url') cancelPendingSeek(urlVideo);
  urlVideo = null;

  const url = new URL(location.href);
  const times = url.searchParams.getAll('t');
  if (times.length !== 1 || !/^\d+(?:\.\d+)?$/.test(times[0])) return;
  const time = Number(times[0]);
  if (!Number.isFinite(time)) return;
  let id;
  try { id = decodeURIComponent(url.hash.slice(1)); } catch { return; }
  if (!id) return;
  const video = document.getElementById(id);
  if (!(video instanceof HTMLVideoElement)) return;
  const chapter = [...document.querySelectorAll('button[data-video-target][data-video-time]')]
    .some((button) => button.dataset.videoTarget === id
      && /^\d+(?:\.\d+)?$/.test(button.dataset.videoTime)
      && Number(button.dataset.videoTime) === time);
  if (!chapter) return;
  urlVideo = video;
  seekVideo(video, time, 'url');
}

window.addEventListener('hashchange', seekUrlChapter);
window.addEventListener('popstate', seekUrlChapter);
if (document.readyState === 'loading') {
  document.addEventListener('DOMContentLoaded', seekUrlChapter, { once: true });
} else {
  seekUrlChapter();
}
