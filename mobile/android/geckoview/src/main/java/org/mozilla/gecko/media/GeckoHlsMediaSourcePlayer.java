/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

package org.mozilla.gecko.media;

import android.os.Handler;
import android.os.Looper;
import android.os.SystemClock;
import android.util.Log;
import android.util.Pair;
import androidx.annotation.Nullable;
import androidx.annotation.OptIn;
import androidx.media3.common.C;
import androidx.media3.common.Format;
import androidx.media3.common.PlaybackException;
import androidx.media3.common.Player;
import androidx.media3.common.Timeline;
import androidx.media3.common.util.Clock;
import androidx.media3.common.util.UnstableApi;
import androidx.media3.common.util.Util;
import androidx.media3.exoplayer.ExoPlaybackException;
import androidx.media3.exoplayer.LoadControl;
import androidx.media3.exoplayer.LoadingInfo;
import androidx.media3.exoplayer.Renderer;
import androidx.media3.exoplayer.RendererCapabilities;
import androidx.media3.exoplayer.StandaloneMediaClock;
import androidx.media3.exoplayer.analytics.PlayerId;
import androidx.media3.exoplayer.source.MediaPeriod;
import androidx.media3.exoplayer.source.MediaSource;
import androidx.media3.exoplayer.source.SampleStream;
import androidx.media3.exoplayer.trackselection.ExoTrackSelection;
import androidx.media3.exoplayer.trackselection.TrackSelector;
import androidx.media3.exoplayer.trackselection.TrackSelectorResult;
import androidx.media3.exoplayer.upstream.BandwidthMeter;
import java.io.IOException;

/**
 * Plays a Media3 {@link MediaSource} through a set of renderers without creating an ExoPlayer.
 *
 * <p>ExoPlayer can't be used in isolated processes: its constructor generates an audio session ID,
 * which blocks forever there because the audio service can't be looked up. This class does the
 * parts of ExoPlayer's internal playback loop that {@link GeckoHlsPlayer} relies on, using the same
 * Media3 components (source, track selector, load control and renderers), and exposes the subset of
 * the ExoPlayer API that GeckoHlsPlayer uses, with the same semantics. It supports a single media
 * source with a single period, which is all HLS needs.
 *
 * <p>All methods must be called on the thread of the looper passed to the constructor, and listener
 * methods are called on it.
 */
@OptIn(markerClass = UnstableApi.class)
/* package */ final class GeckoHlsMediaSourcePlayer {
  private static final String LOGTAG = "GeckoHlsMediaSourcePlayer";

  // The same intervals ExoPlayer's playback loop uses.
  private static final long ACTIVE_INTERVAL_MS = 10;
  private static final long IDLE_INTERVAL_MS = 1000;

  private final Handler mHandler;
  private final Renderer[] mRenderers;
  private final RendererCapabilities[] mRendererCapabilities;
  private final TrackSelector mTrackSelector;
  private final LoadControl mLoadControl;
  private final BandwidthMeter mBandwidthMeter;
  private final PlayerId mPlayerId = PlayerId.UNSET;
  private final StandaloneMediaClock mClock = new StandaloneMediaClock(Clock.DEFAULT);
  private final Runnable mWork = this::doWork;
  private final MediaSource.MediaSourceCaller mSourceCaller = this::onSourceInfoRefreshed;
  private final MediaPeriod.Callback mPeriodCallback =
      new MediaPeriod.Callback() {
        @Override
        public void onPrepared(final MediaPeriod mediaPeriod) {
          onPeriodPrepared();
        }

        @Override
        public void onContinueLoadingRequested(final MediaPeriod source) {
          maybeContinueLoading();
        }
      };

  private @Nullable Player.Listener mListener;
  private @Nullable MediaSource mSource;
  private @Nullable Timeline mTimeline;
  private @Nullable MediaSource.MediaPeriodId mPeriodId;
  private @Nullable MediaPeriod mPeriod;
  private @Nullable ExoTrackSelection[] mSelections;
  private final boolean[] mRendererEnabled;
  private boolean mPeriodPrepared = false;
  // Whether the period is the last one, so renderers should be told about the end of its streams.
  private boolean mIsFinal = false;
  // A seek requested before the source was prepared, as a window position.
  private long mPendingWindowPositionUs = C.TIME_UNSET;
  private long mStartPositionUs = 0;
  private boolean mPlayWhenReady = false;
  private @Player.State int mState = Player.STATE_IDLE;
  // As in ExoPlayer, whether the player wants to load more, rather than whether a load is in
  // progress. Loads pause briefly while the source loads playlists, for example after excluding a
  // failing variant.
  private boolean mIsLoading = false;
  private boolean mShouldContinueLoading = false;
  private boolean mReleased = false;

  GeckoHlsMediaSourcePlayer(
      final Looper looper,
      final Renderer[] renderers,
      final TrackSelector trackSelector,
      final LoadControl loadControl,
      final BandwidthMeter bandwidthMeter) {
    mHandler = new Handler(looper);
    mRenderers = renderers;
    mTrackSelector = trackSelector;
    mLoadControl = loadControl;
    mBandwidthMeter = bandwidthMeter;
    mRendererCapabilities = new RendererCapabilities[renderers.length];
    mRendererEnabled = new boolean[renderers.length];
    for (int i = 0; i < renderers.length; i++) {
      renderers[i].init(i, mPlayerId, Clock.DEFAULT);
      mRendererCapabilities[i] = renderers[i].getCapabilities();
    }
    trackSelector.init(parameters -> {}, bandwidthMeter);
  }

  public void addListener(final Player.Listener listener) {
    mListener = listener;
  }

  public void removeListener(final Player.Listener listener) {
    if (mListener == listener) {
      mListener = null;
    }
  }

  public void setMediaSource(final MediaSource source) {
    mSource = source;
  }

  public void prepare() {
    mLoadControl.onPrepared(mPlayerId);
    mSource.prepareSource(mSourceCaller, mPlayerId, mBandwidthMeter);
    setState(Player.STATE_BUFFERING);
    scheduleWork(0);
  }

  public void setPlayWhenReady(final boolean playWhenReady) {
    mPlayWhenReady = playWhenReady;
    updateClock();
    scheduleWork(0);
  }

  public boolean getPlayWhenReady() {
    return mPlayWhenReady;
  }

  public @Player.State int getPlaybackState() {
    return mState;
  }

  // In milliseconds, relative to the start of the window, like ExoPlayer.getDuration().
  public long getDuration() {
    if (mTimeline == null || mTimeline.isEmpty()) {
      return C.TIME_UNSET;
    }
    return mTimeline.getWindow(0, new Timeline.Window()).getDurationMs();
  }

  // In milliseconds, relative to the start of the window, like ExoPlayer.getBufferedPosition().
  public long getBufferedPosition() {
    if (!mPeriodPrepared) {
      return 0;
    }
    final long bufferedUs = mPeriod.getBufferedPositionUs();
    if (bufferedUs == C.TIME_END_OF_SOURCE) {
      final long durationMs = getDuration();
      return durationMs == C.TIME_UNSET ? 0 : durationMs;
    }
    return Util.usToMs(Math.max(0, bufferedUs - windowOffsetUs()));
  }

  // In milliseconds, relative to the start of the window, like ExoPlayer.seekTo().
  public void seekTo(final long positionMs) {
    if (!mPeriodPrepared) {
      mPendingWindowPositionUs = Util.msToUs(positionMs);
      return;
    }
    final long actualPositionUs = mPeriod.seekToUs(Util.msToUs(positionMs) + windowOffsetUs());
    for (int i = 0; i < mRenderers.length; i++) {
      if (mRendererEnabled[i]) {
        try {
          mRenderers[i].resetPosition(actualPositionUs, /* sampleStreamIsResetToKeyFrame= */ false);
          // Resetting the position clears this, as ExoPlayer sets it again after seeking.
          if (mIsFinal) {
            mRenderers[i].setCurrentStreamFinal();
          }
        } catch (final ExoPlaybackException e) {
          fail(e);
          return;
        }
      }
    }
    mClock.resetPosition(actualPositionUs);
    setState(Player.STATE_BUFFERING);
    scheduleWork(0);
  }

  public void stop() {
    mHandler.removeCallbacks(mWork);
    for (int i = 0; i < mRenderers.length; i++) {
      if (mRendererEnabled[i]) {
        mRenderers[i].stop();
        mRenderers[i].disable();
        mRendererEnabled[i] = false;
      }
    }
    mClock.stop();
    mLoadControl.onStopped(mPlayerId);
  }

  public void release() {
    if (mReleased) {
      return;
    }
    mReleased = true;
    stop();
    if (mSelections != null) {
      for (final ExoTrackSelection selection : mSelections) {
        if (selection != null) {
          selection.disable();
        }
      }
    }
    if (mSource != null) {
      if (mPeriod != null) {
        mSource.releasePeriod(mPeriod);
        mPeriod = null;
      }
      mSource.releaseSource(mSourceCaller);
    }
    for (final Renderer renderer : mRenderers) {
      renderer.reset();
      renderer.release();
    }
    mTrackSelector.release();
    mLoadControl.onReleased(mPlayerId);
    mListener = null;
  }

  private long windowOffsetUs() {
    if (mTimeline == null || mTimeline.isEmpty()) {
      return 0;
    }
    return mTimeline.getWindow(0, new Timeline.Window()).positionInFirstPeriodUs;
  }

  private void onSourceInfoRefreshed(final MediaSource source, final Timeline timeline) {
    mTimeline = timeline;
    if (mListener != null) {
      mListener.onTimelineChanged(timeline, Player.TIMELINE_CHANGE_REASON_SOURCE_UPDATE);
    }
    if (mPeriod == null && !timeline.isEmpty()) {
      final Timeline.Window window = timeline.getWindow(0, new Timeline.Window());
      // Without a pending seek, start where ExoPlayer would: the window's default position, which
      // honours EXT-X-START and the live edge.
      final Pair<Object, Long> start =
          timeline.getPeriodPositionUs(window, new Timeline.Period(), 0, mPendingWindowPositionUs);
      mPeriodId = new MediaSource.MediaPeriodId(start.first);
      mStartPositionUs = start.second;
      mPeriod =
          source.createPeriod(mPeriodId, mLoadControl.getAllocator(mPlayerId), mStartPositionUs);
      mPeriod.prepare(mPeriodCallback, mStartPositionUs);
    }
    scheduleWork(0);
  }

  private void onPeriodPrepared() {
    try {
      final TrackSelectorResult result =
          mTrackSelector.selectTracks(
              mRendererCapabilities, mPeriod.getTrackGroups(), mPeriodId, mTimeline);
      mTrackSelector.onSelectionActivated(result.info);
      mSelections = result.selections;
      for (final ExoTrackSelection selection : mSelections) {
        if (selection != null) {
          selection.enable();
        }
      }
      final SampleStream[] streams = new SampleStream[mRenderers.length];
      final long positionUs =
          mPeriod.selectTracks(
              mSelections,
              new boolean[mRenderers.length],
              streams,
              new boolean[mRenderers.length],
              mStartPositionUs);
      mLoadControl.onTracksSelected(
          loadControlParameters(positionUs, /* bufferedDurationUs= */ 0),
          mPeriod.getTrackGroups(),
          mSelections);

      mIsFinal = !mTimeline.getWindow(0, new Timeline.Window()).isDynamic;
      for (int i = 0; i < mRenderers.length; i++) {
        if (!result.isRendererEnabled(i) || streams[i] == null) {
          continue;
        }
        final ExoTrackSelection selection = mSelections[i];
        final Format[] formats = new Format[selection.length()];
        for (int j = 0; j < formats.length; j++) {
          formats[j] = selection.getFormat(j);
        }
        mRenderers[i].enable(
            result.rendererConfigurations[i],
            formats,
            streams[i],
            positionUs,
            /* joining= */ false,
            /* mayRenderStartOfStream= */ true,
            positionUs,
            /* offsetUs= */ 0,
            mPeriodId);
        if (mIsFinal) {
          // Lets the renderers see the end of the stream.
          mRenderers[i].setCurrentStreamFinal();
        }
        mRenderers[i].start();
        mRendererEnabled[i] = true;
      }
      mClock.resetPosition(positionUs);
      mPeriodPrepared = true;
      if (mListener != null) {
        mListener.onTracksChanged(result.tracks);
      }
    } catch (final ExoPlaybackException e) {
      fail(e);
      return;
    }
    maybeContinueLoading();
    scheduleWork(0);
  }

  private void scheduleWork(final long delayMs) {
    if (mReleased) {
      return;
    }
    mHandler.removeCallbacks(mWork);
    mHandler.postDelayed(mWork, delayMs);
  }

  // One iteration of the playback loop, as in ExoPlayerImplInternal.doSomeWork().
  private void doWork() {
    if (mReleased || mState == Player.STATE_IDLE) {
      return;
    }
    try {
      if (!mPeriodPrepared) {
        mSource.maybeThrowSourceInfoRefreshError();
        if (mPeriod != null) {
          mPeriod.maybeThrowPrepareError();
        }
      } else {
        mPeriod.readDiscontinuity();
        final long positionUs = mClock.getPositionUs();
        final long elapsedRealtimeUs = SystemClock.elapsedRealtime() * 1000;
        boolean allReady = true;
        boolean allEnded = true;
        for (int i = 0; i < mRenderers.length; i++) {
          if (!mRendererEnabled[i]) {
            continue;
          }
          final Renderer renderer = mRenderers[i];
          renderer.render(positionUs, elapsedRealtimeUs);
          renderer.maybeThrowStreamError();
          allReady &= renderer.isReady();
          allEnded &= renderer.isEnded();
        }
        mSource.maybeThrowSourceInfoRefreshError();
        mPeriod.discardBuffer(
            positionUs - mLoadControl.getBackBufferDurationUs(mPlayerId),
            mLoadControl.retainBackBufferFromKeyframe(mPlayerId));
        mPeriod.reevaluateBuffer(positionUs);
        setState(
            allEnded ? Player.STATE_ENDED : allReady ? Player.STATE_READY : Player.STATE_BUFFERING);
      }
      maybeContinueLoading();
    } catch (final ExoPlaybackException e) {
      fail(e);
      return;
    } catch (final IOException e) {
      fail(ExoPlaybackException.createForSource(e, PlaybackException.ERROR_CODE_IO_UNSPECIFIED));
      return;
    } catch (final RuntimeException e) {
      fail(ExoPlaybackException.createForUnexpected(e, PlaybackException.ERROR_CODE_UNSPECIFIED));
      return;
    }

    final boolean isLoading = mShouldContinueLoading;
    if (isLoading != mIsLoading) {
      mIsLoading = isLoading;
      if (mListener != null) {
        mListener.onIsLoadingChanged(isLoading);
      }
    }
    if (mState == Player.STATE_ENDED) {
      return;
    }
    scheduleWork(
        mState == Player.STATE_READY && !mPlayWhenReady ? IDLE_INTERVAL_MS : ACTIVE_INTERVAL_MS);
  }

  private void maybeContinueLoading() {
    if (mPeriod == null || mReleased) {
      return;
    }
    final long positionUs = mClock.getPositionUs();
    if (mPeriodPrepared) {
      final long nextLoadPositionUs = mPeriod.getNextLoadPositionUs();
      mShouldContinueLoading =
          nextLoadPositionUs != C.TIME_END_OF_SOURCE
              && mLoadControl.shouldContinueLoading(
                  loadControlParameters(positionUs, Math.max(0, nextLoadPositionUs - positionUs)));
    } else {
      // Before preparation completes, this lets the period load what it needs to build its
      // tracks.
      mShouldContinueLoading = true;
    }
    if (mShouldContinueLoading) {
      mPeriod.continueLoading(
          new LoadingInfo.Builder().setPlaybackPositionUs(positionUs).setPlaybackSpeed(1f).build());
    }
  }

  private LoadControl.Parameters loadControlParameters(
      final long positionUs, final long bufferedDurationUs) {
    return new LoadControl.Parameters(
        mPlayerId,
        mTimeline,
        mPeriodId,
        positionUs,
        bufferedDurationUs,
        /* playbackSpeed= */ 1f,
        mPlayWhenReady,
        /* rebuffering= */ false,
        /* targetLiveOffsetUs= */ C.TIME_UNSET,
        /* lastRebufferRealtimeMs= */ C.TIME_UNSET);
  }

  private void setState(final @Player.State int state) {
    if (state == mState) {
      return;
    }
    mState = state;
    updateClock();
    if (mListener != null) {
      mListener.onPlaybackStateChanged(state);
    }
  }

  private void updateClock() {
    if (mPlayWhenReady && mState == Player.STATE_READY) {
      mClock.start();
    } else {
      mClock.stop();
    }
  }

  private void fail(final ExoPlaybackException error) {
    Log.e(LOGTAG, "Playback failed", error);
    mHandler.removeCallbacks(mWork);
    setState(Player.STATE_IDLE);
    if (mListener != null) {
      mListener.onPlayerError(error);
    }
  }
}
