// Shared by the marketing walkthrough and first-session documentation.
const pendingSeeks = new WeakMap();

document.addEventListener('click', (event) => {
  const button = event.target.closest('button[data-video-target][data-video-time]');
  if (!button) return;
  const video = document.getElementById(button.dataset.videoTarget);
  const time = Number(button.dataset.videoTime);
  if (!(video instanceof HTMLVideoElement) || !Number.isFinite(time) || time < 0) return;

  // A second chapter click before metadata arrives replaces the first request.
  const pending = pendingSeeks.get(video);
  if (pending) video.removeEventListener('loadedmetadata', pending);
  const seek = () => {
    video.currentTime = Number.isFinite(video.duration) ? Math.min(time, video.duration) : time;
    pendingSeeks.delete(video);
  };
  if (video.readyState >= HTMLMediaElement.HAVE_METADATA) {
    seek();
  } else {
    pendingSeeks.set(video, seek);
    video.addEventListener('loadedmetadata', seek, { once: true });
  }

  // Start within the user gesture, including when the media still needs to load.
  // Native controls remain available if the browser declines playback.
  video.play().catch(() => {});
  video.scrollIntoView({ block: 'nearest', behavior: 'instant' });
});
