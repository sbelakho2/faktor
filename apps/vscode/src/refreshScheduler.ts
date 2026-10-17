// Bounded refresh coalescing for the durable event stream.
//
// A busy journal can emit hundreds of frames per second. Each frame only
// proves that SOME durable state changed; one bounded page refresh observes
// all of it, so frames are coalesced instead of refreshing per frame. The
// scheduler is leading-edge-ish and fixed-rate (the first request arms one
// timer; later requests never push it out), which avoids the classic
// trailing-debounce starvation where a continuous stream never refreshes.
// A request that arrives while a refresh is running is remembered and starts
// the next window when the run reports done, so the newest state is never
// left unread.

export interface SchedulerTimers {
  setTimeout(callback: () => void, ms: number): unknown;
  clearTimeout(handle: unknown): void;
}

export type RefreshRun = (done: () => void) => void;

export const DEFAULT_REFRESH_COALESCE_MS = 120;

export class CoalescingRefreshScheduler {
  private timer: unknown | null = null;
  private running = false;
  private dirty = false;
  private disposed = false;
  private readonly run: RefreshRun;
  private readonly timers: SchedulerTimers;
  private readonly coalesceMs: number;

  constructor(
    run: RefreshRun,
    timers: SchedulerTimers,
    coalesceMs: number = DEFAULT_REFRESH_COALESCE_MS,
  ) {
    this.run = run;
    this.timers = timers;
    this.coalesceMs = coalesceMs;
  }

  /** Request one refresh in the current coalescing window. */
  request(): void {
    if (this.disposed) {
      return;
    }
    this.dirty = true;
    if (this.running || this.timer !== null) {
      return;
    }
    this.arm();
  }

  /** True while a timer is armed or a refresh is running. */
  get busy(): boolean {
    return this.running || this.timer !== null;
  }

  /** Drop an armed timer (session switch / daemon stop). */
  reset(): void {
    if (this.timer !== null) {
      this.timers.clearTimeout(this.timer);
      this.timer = null;
    }
    this.dirty = false;
  }

  dispose(): void {
    this.reset();
    this.disposed = true;
  }

  private arm(): void {
    this.timer = this.timers.setTimeout(() => {
      this.timer = null;
      this.fire();
    }, this.coalesceMs);
  }

  private fire(): void {
    if (this.disposed) {
      return;
    }
    if (this.running) {
      this.dirty = true;
      if (this.timer === null) {
        this.arm();
      }
      return;
    }
    if (!this.dirty) {
      return;
    }
    this.dirty = false;
    this.running = true;
    let finished = false;
    const done = (): void => {
      if (finished) {
        return;
      }
      finished = true;
      this.running = false;
      if (this.dirty && !this.disposed) {
        this.arm();
      }
    };
    try {
      this.run(done);
    } catch (error) {
      done();
      throw error;
    }
  }
}

export const nodeSchedulerTimers: SchedulerTimers = {
  setTimeout: (callback, ms) => setTimeout(callback, ms),
  clearTimeout: (handle) => clearTimeout(handle as ReturnType<typeof setTimeout>),
};
