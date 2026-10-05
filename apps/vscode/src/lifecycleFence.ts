/**
 * Lifecycle generation fence (audit P1-VSCODE). Async host work (the daemon
 * refresh chain, session rebinds, daemon restarts) captures `current()`
 * before its first await and must refuse to publish host state unless the
 * generation is still current when it finishes. `bump()` marks every
 * captured generation stale. Pure and synchronous so the move-forward
 * property is unit-testable without a VS Code host.
 */
export class LifecycleFence {
  private generation = 0;

  /** Invalidate every previously captured generation. */
  bump(): void {
    this.generation += 1;
  }

  /** The generation a new async operation must capture before awaiting. */
  current(): number {
    return this.generation;
  }

  /** TRUE only while `generation` has not been invalidated by a bump. */
  isCurrent(generation: number): boolean {
    return generation === this.generation;
  }
}
