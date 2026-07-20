export const motionDuration = {
  press: 120,
  fast: 150,
  standard: 200,
  emphasis: 260,
} as const;

export const motionEasing = {
  out: "cubic-bezier(0.23, 1, 0.32, 1)",
  inOut: "cubic-bezier(0.77, 0, 0.175, 1)",
  drawer: "cubic-bezier(0.32, 0.72, 0, 1)",
} as const;

export type LayoutSnapshot = Map<string, DOMRect>;

const reducedMotion = window.matchMedia("(prefers-reduced-motion: reduce)");
const activeAnimations = new WeakMap<Element, Animation>();

export function shouldReduceMotion(): boolean {
  return reducedMotion.matches;
}

export function shouldAnimateInteraction(event?: Event): boolean {
  if (shouldReduceMotion()) return false;
  if (event instanceof KeyboardEvent) return false;
  return !(event instanceof MouseEvent && event.detail === 0);
}

export function playMotion(
  target: Element,
  keyframes: Keyframe[],
  options: KeyframeAnimationOptions,
  persist = false,
): Animation | null {
  activeAnimations.get(target)?.cancel();
  if (shouldReduceMotion() || typeof target.animate !== "function") return null;
  const animation = target.animate(keyframes, {
    fill: "both",
    ...options,
  });
  activeAnimations.set(target, animation);
  void animation.finished.catch(() => undefined).then(() => {
    if (activeAnimations.get(target) === animation) {
      activeAnimations.delete(target);
      if (!persist) animation.cancel();
    }
  });
  return animation;
}

export async function waitForMotion(animation: Animation | null): Promise<void> {
  if (!animation) return;
  try {
    await animation.finished;
  } catch {
    // A new target replaces the current animation. Cancellation is expected.
  }
}

export function animateEntrance(
  target: Element,
  delay = 0,
  distance = 8,
): Animation | null {
  return playMotion(target, [
    { opacity: 0, transform: `translateY(${distance}px)` },
    { opacity: 1, transform: "translateY(0)" },
  ], {
    duration: motionDuration.standard,
    delay,
    easing: motionEasing.out,
  });
}

export function animateStateChange(target: Element): Animation | null {
  return playMotion(target, [
    { opacity: 0.68, transform: "translateY(3px)" },
    { opacity: 1, transform: "translateY(0)" },
  ], {
    duration: motionDuration.fast,
    easing: motionEasing.out,
  });
}

export function revealApplication(boot: HTMLElement, app: HTMLElement): void {
  app.hidden = false;
  if (shouldReduceMotion()) {
    boot.hidden = true;
    return;
  }

  playMotion(app, [
    { opacity: 0, transform: "scale(0.992)" },
    { opacity: 1, transform: "scale(1)" },
  ], {
    duration: motionDuration.emphasis,
    easing: motionEasing.out,
  });
  const bootExit = playMotion(boot, [
    { opacity: 1, transform: "scale(1)" },
    { opacity: 0, transform: "scale(1.015)" },
  ], {
    duration: motionDuration.standard,
    easing: motionEasing.out,
  });
  void waitForMotion(bootExit).then(() => {
    boot.hidden = true;
  });

  const staged = [
    app.querySelector(".topbar"),
    app.querySelector(".station-sidebar"),
    app.querySelector(".hero-card"),
    app.querySelector(".content-section"),
    app.querySelector(".details-grid"),
    app.querySelector(".runtime-toggle"),
  ].filter((element): element is Element => element !== null);
  staged.forEach((element, index) => animateEntrance(element, 30 + index * 34, index < 2 ? 5 : 9));
  app.querySelectorAll(".metric-grid > div").forEach((element, index) => {
    animateEntrance(element, 95 + index * 18, 4);
  });
  app.querySelectorAll(".forecast-day").forEach((element, index) => {
    animateEntrance(element, 115 + index * 26, 6);
  });
}

export function animateBootState(...targets: HTMLElement[]): void {
  targets.filter((target) => !target.hidden).forEach((target, index) => {
    playMotion(target, [{ opacity: 0 }, { opacity: 1 }], {
      duration: motionDuration.standard,
      delay: index * 35,
      easing: motionEasing.out,
    });
  });
}

export function animateDialogIn(dialog: HTMLDialogElement): Animation | null {
  return playMotion(dialog, [
    { opacity: 0, transform: "translateY(10px) scale(0.97)" },
    { opacity: 1, transform: "translateY(0) scale(1)" },
  ], {
    duration: motionDuration.standard,
    easing: motionEasing.out,
  });
}

export function animateDialogOut(dialog: HTMLDialogElement): Animation | null {
  return playMotion(dialog, [
    { opacity: 1, transform: "translateY(0) scale(1)" },
    { opacity: 0, transform: "translateY(5px) scale(0.985)" },
  ], {
    duration: motionDuration.fast,
    easing: motionEasing.out,
  });
}

export function animateOverlayIn(overlay: HTMLElement, panel: HTMLElement): void {
  playMotion(overlay, [{ opacity: 0 }, { opacity: 1 }], {
    duration: motionDuration.standard,
    easing: motionEasing.out,
  });
  playMotion(panel, [
    { opacity: 0, transform: "translateY(10px) scale(0.97)" },
    { opacity: 1, transform: "translateY(0) scale(1)" },
  ], {
    duration: motionDuration.emphasis,
    easing: motionEasing.out,
  });
}

export async function animateOverlayOut(overlay: HTMLElement, panel: HTMLElement): Promise<void> {
  playMotion(overlay, [{ opacity: 1 }, { opacity: 0 }], {
    duration: motionDuration.fast,
    easing: motionEasing.out,
  });
  await waitForMotion(playMotion(panel, [
    { opacity: 1, transform: "translateY(0) scale(1)" },
    { opacity: 0, transform: "translateY(5px) scale(0.985)" },
  ], {
    duration: motionDuration.fast,
    easing: motionEasing.out,
  }));
}

export function captureLayout(container: Element, selector: string): LayoutSnapshot {
  const positions: LayoutSnapshot = new Map();
  container.querySelectorAll<HTMLElement>(selector).forEach((element) => {
    const key = element.dataset.motionKey;
    if (key) positions.set(key, element.getBoundingClientRect());
  });
  return positions;
}

export function animateLayoutFrom(
  container: Element,
  selector: string,
  previous: LayoutSnapshot | null,
): void {
  if (!previous || shouldReduceMotion()) return;
  container.querySelectorAll<HTMLElement>(selector).forEach((element, index) => {
    const key = element.dataset.motionKey;
    const before = key ? previous.get(key) : undefined;
    if (!before) {
      animateEntrance(element, Math.min(index * 24, 96), 5);
      return;
    }
    const after = element.getBoundingClientRect();
    const deltaX = before.left - after.left;
    const deltaY = before.top - after.top;
    if (Math.abs(deltaX) < 0.5 && Math.abs(deltaY) < 0.5) return;
    playMotion(element, [
      { transform: `translate(${deltaX}px, ${deltaY}px)` },
      { transform: "translate(0, 0)" },
    ], {
      duration: motionDuration.emphasis,
      easing: motionEasing.drawer,
    });
  });
}

export function animateChartPaths(paths: SVGGeometryElement[], fadeTargets: Element[] = []): void {
  if (shouldReduceMotion()) return;
  paths.forEach((path, index) => {
    const length = path.getTotalLength();
    if (!Number.isFinite(length) || length <= 0) return;
    path.style.strokeDasharray = String(length);
    const animation = playMotion(path, [
      { strokeDashoffset: length, opacity: 0.45 },
      { strokeDashoffset: 0, opacity: 1 },
    ], {
      duration: 380,
      delay: index * 35,
      easing: motionEasing.out,
    });
    void waitForMotion(animation).then(() => {
      path.style.removeProperty("stroke-dasharray");
      path.style.removeProperty("stroke-dashoffset");
    });
  });
  fadeTargets.forEach((target) => {
    playMotion(target, [{ opacity: 0 }, { opacity: 1 }], {
      duration: motionDuration.emphasis,
      easing: motionEasing.out,
    });
  });
}

export function animateApplicationExit(app: HTMLElement): void {
  document.documentElement.dataset.exiting = "true";
  playMotion(app, [
    { opacity: 1, transform: "scale(1)" },
    { opacity: 0.72, transform: "scale(0.992)" },
  ], {
    duration: 180,
    easing: motionEasing.inOut,
  }, true);
}
