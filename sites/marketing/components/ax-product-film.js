let nextFilmId = 0;

class AxProductFilm extends HTMLElement {
  connectedCallback() {
    if (this._initialized) return;
    this._initialized = true;

    const film = this.getAttribute('film');
    const src = this.getAttribute('src');
    const poster = this.getAttribute('poster');
    const label = this.getAttribute('label');
    const caption = this.getAttribute('caption');

    const validFilm = /^[a-z0-9]+(?:-[a-z0-9]+)*$/.test(film || '');
    const expectedSrc = validFilm ? `/assets/films/${film}.mp4` : null;
    const expectedPoster = validFilm ? `/assets/films/${film}.jpg` : null;
    if (!validFilm || src !== expectedSrc || poster !== expectedPoster || !label || !caption) {
      this.dataset.state = 'error';
      console.error('ax-product-film requires a film slug, its matching MP4/JPEG pair, a label, and a caption.');
      return;
    }

    this.dataset.film = film;

    const id = `product-film-${++nextFilmId}`;
    const figure = document.createElement('figure');
    const frame = document.createElement('div');
    const video = document.createElement('video');
    const actions = document.createElement('div');
    const control = document.createElement('button');
    const openControl = document.createElement('button');
    const figcaption = document.createElement('figcaption');

    figure.className = 'product-film';
    frame.className = 'product-film-frame';
    video.className = 'product-film-video';
    video.id = id;
    video.muted = true;
    video.defaultMuted = true;
    video.playsInline = true;
    video.setAttribute('muted', '');
    video.setAttribute('playsinline', '');
    video.preload = this.getAttribute('preload') === 'metadata' ? 'metadata' : 'none';
    video.poster = poster;
    video.src = src;
    video.setAttribute('aria-label', label);

    actions.className = 'product-film-actions';
    actions.setAttribute('role', 'group');
    actions.setAttribute('aria-label', `Film controls: ${label}`);

    control.className = 'product-film-control';
    control.type = 'button';
    control.setAttribute('aria-controls', id);
    control.textContent = 'Play film';
    control.setAttribute('aria-label', `Play product film: ${label}`);

    openControl.className = 'product-film-open';
    openControl.type = 'button';
    openControl.setAttribute('aria-controls', id);
    openControl.setAttribute('aria-expanded', 'false');
    openControl.textContent = 'Open film';
    openControl.setAttribute('aria-label', `Open product film full screen: ${label}`);

    figcaption.className = 'product-film-caption';
    figcaption.id = `${id}-caption`;
    figcaption.textContent = caption;
    video.setAttribute('aria-describedby', figcaption.id);

    actions.append(control, openControl);
    frame.append(video, actions);
    figure.append(frame, figcaption);
    this.replaceChildren(figure);

    this._frame = frame;
    this._video = video;
    this._control = control;
    this._openControl = openControl;
    this._label = label;
    this._src = src;
    this._manualPause = false;
    this._pausedOffscreen = false;
    this._autoplayAttempted = false;
    this._motionQuery = window.matchMedia('(prefers-reduced-motion: reduce)');
    this.dataset.state = 'paused';

    this._onControlClick = () => {
      if (video.ended) video.currentTime = 0;

      if (video.paused) {
        this._manualPause = false;
        this._pausedOffscreen = false;
        this._play();
      } else {
        this._manualPause = true;
        video.pause();
      }
    };

    this._onOpenClick = async () => {
      const fullscreenElement = document.fullscreenElement || document.webkitFullscreenElement;
      if (fullscreenElement === frame) {
        await this._exitFullscreen();
        return;
      }

      try {
        if (frame.requestFullscreen) {
          await frame.requestFullscreen();
        } else if (frame.webkitRequestFullscreen) {
          frame.webkitRequestFullscreen();
        } else if (video.webkitEnterFullscreen) {
          video.webkitEnterFullscreen();
        } else {
          this._openStandaloneFilm();
        }
      } catch {
        this._openStandaloneFilm();
      }
    };

    this._onPlay = () => this._setControl('Pause film', `Pause product film: ${label}`, 'playing');
    this._onPause = () => {
      if (!video.ended) this._setControl('Play film', `Play product film: ${label}`, 'paused');
    };
    this._onEnded = () => this._setControl('Replay film', `Replay product film: ${label}`, 'ended');
    this._onError = () => {
      control.disabled = true;
      openControl.disabled = true;
      this._setControl('Film unavailable', `Product film unavailable: ${label}`, 'error');
    };
    this._onFullscreenChange = () => {
      const fullscreenElement = document.fullscreenElement || document.webkitFullscreenElement;
      this._setOpenControl(fullscreenElement === frame);
    };
    this._onNativeFullscreenStart = () => this._setOpenControl(true);
    this._onNativeFullscreenEnd = () => this._setOpenControl(false);
    this._onMotionChange = (event) => {
      this.toggleAttribute('data-reduced-motion', event.matches);
      if (event.matches && !video.paused) {
        this._manualPause = true;
        video.pause();
      }
    };

    control.addEventListener('click', this._onControlClick);
    openControl.addEventListener('click', this._onOpenClick);
    video.addEventListener('play', this._onPlay);
    video.addEventListener('pause', this._onPause);
    video.addEventListener('ended', this._onEnded);
    video.addEventListener('error', this._onError);
    video.addEventListener('webkitbeginfullscreen', this._onNativeFullscreenStart);
    video.addEventListener('webkitendfullscreen', this._onNativeFullscreenEnd);
    document.addEventListener('fullscreenchange', this._onFullscreenChange);
    document.addEventListener('webkitfullscreenchange', this._onFullscreenChange);
    if (this._motionQuery.addEventListener) this._motionQuery.addEventListener('change', this._onMotionChange);
    else this._motionQuery.addListener(this._onMotionChange);
    this.toggleAttribute('data-reduced-motion', this._motionQuery.matches);

    if ('IntersectionObserver' in window) {
      this._observer = new IntersectionObserver((entries) => this._onIntersection(entries), {
        threshold: [0, 0.35],
      });
      this._observer.observe(this);
    } else if (this.hasAttribute('autoplay') && !this._motionQuery.matches) {
      this._autoplayAttempted = true;
      this._play();
    }
  }

  disconnectedCallback() {
    this._observer?.disconnect();
    this._video?.pause();
    this._control?.removeEventListener('click', this._onControlClick);
    this._openControl?.removeEventListener('click', this._onOpenClick);
    this._video?.removeEventListener('play', this._onPlay);
    this._video?.removeEventListener('pause', this._onPause);
    this._video?.removeEventListener('ended', this._onEnded);
    this._video?.removeEventListener('error', this._onError);
    this._video?.removeEventListener('webkitbeginfullscreen', this._onNativeFullscreenStart);
    this._video?.removeEventListener('webkitendfullscreen', this._onNativeFullscreenEnd);
    document.removeEventListener('fullscreenchange', this._onFullscreenChange);
    document.removeEventListener('webkitfullscreenchange', this._onFullscreenChange);
    if (this._motionQuery?.removeEventListener) this._motionQuery.removeEventListener('change', this._onMotionChange);
    else this._motionQuery?.removeListener(this._onMotionChange);
    this._initialized = false;
  }

  _onIntersection(entries) {
    const entry = entries[entries.length - 1];
    const visible = entry.isIntersecting && entry.intersectionRatio >= 0.35;

    if (!visible) {
      if (!this._video.paused && !this._video.ended) {
        this._pausedOffscreen = true;
        this._video.pause();
      }
      return;
    }

    if (this._motionQuery.matches || this._manualPause || this._video.ended) return;

    if (this._pausedOffscreen) {
      this._pausedOffscreen = false;
      this._play();
      return;
    }

    if (this.hasAttribute('autoplay') && !this._autoplayAttempted) {
      this._autoplayAttempted = true;
      this._play();
    }
  }

  async _play() {
    try {
      await this._video.play();
    } catch {
      this._setControl('Play film', `Play product film: ${this._label}`, 'paused');
    }
  }

  async _exitFullscreen() {
    if (document.exitFullscreen) {
      await document.exitFullscreen();
    } else if (document.webkitExitFullscreen) {
      document.webkitExitFullscreen();
    }
  }

  _openStandaloneFilm() {
    const opened = window.open(this._src, '_blank', 'noopener,noreferrer');
    if (opened) opened.opener = null;
  }

  _setOpenControl(open) {
    this.toggleAttribute('data-open', open);
    this._openControl.textContent = open ? 'Close film' : 'Open film';
    this._openControl.setAttribute('aria-expanded', String(open));
    this._openControl.setAttribute(
      'aria-label',
      `${open ? 'Close' : 'Open'} product film${open ? '' : ' full screen'}: ${this._label}`,
    );
  }

  _setControl(text, ariaLabel, state) {
    this._control.textContent = text;
    this._control.setAttribute('aria-label', ariaLabel);
    this.dataset.state = state;
  }
}

customElements.define('ax-product-film', AxProductFilm);

function restoreHashPosition() {
  const rawId = window.location.hash.slice(1);
  if (!rawId) return;

  let id = rawId;
  try {
    id = decodeURIComponent(rawId);
  } catch {
    // Keep the literal fragment when it is not valid percent-encoding.
  }

  requestAnimationFrame(() => {
    requestAnimationFrame(() => document.getElementById(id)?.scrollIntoView({ block: 'start' }));
  });
}

restoreHashPosition();
window.addEventListener('hashchange', restoreHashPosition);
