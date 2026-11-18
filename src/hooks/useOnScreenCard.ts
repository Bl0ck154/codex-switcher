import { useEffect, useRef, useState, useSyncExternalStore } from "react";

// One observer and one set of page-visibility listeners for every account card.
// Per-card observers/listeners scale poorly with large account collections.
const observers = new Map<Element, (visible: boolean) => void>();
let intersectionObserver: IntersectionObserver | null = null;
const subscribers = new Set<() => void>();

function notifyVisibility() {
  subscribers.forEach((subscriber) => subscriber());
}

function subscribeVisibility(subscriber: () => void): () => void {
  subscribers.add(subscriber);
  if (subscribers.size === 1 && typeof document !== "undefined") {
    document.addEventListener("visibilitychange", notifyVisibility);
    window.addEventListener("focus", notifyVisibility);
    window.addEventListener("blur", notifyVisibility);
  }
  return () => {
    subscribers.delete(subscriber);
    if (subscribers.size === 0 && typeof document !== "undefined") {
      document.removeEventListener("visibilitychange", notifyVisibility);
      window.removeEventListener("focus", notifyVisibility);
      window.removeEventListener("blur", notifyVisibility);
    }
  };
}

function pageIsVisible(): boolean {
  return typeof document === "undefined" ||
    (document.visibilityState !== "hidden" && document.hasFocus());
}

function observe(element: Element, onChange: (visible: boolean) => void): () => void {
  if (typeof IntersectionObserver === "undefined") {
    onChange(true);
    return () => {};
  }

  if (!intersectionObserver) {
    intersectionObserver = new IntersectionObserver(
      (entries) => {
        for (const entry of entries) {
          observers.get(entry.target)?.(entry.isIntersecting);
        }
      },
      { rootMargin: "160px 0px" },
    );
  }
  observers.set(element, onChange);
  intersectionObserver.observe(element);

  return () => {
    observers.delete(element);
    intersectionObserver?.unobserve(element);
    if (observers.size === 0) {
      intersectionObserver?.disconnect();
      intersectionObserver = null;
    }
  };
}

export function useOnScreenCard() {
  const ref = useRef<HTMLDivElement>(null);
  const [onScreen, setOnScreen] = useState(
    () => typeof IntersectionObserver === "undefined",
  );
  const pageVisible = useSyncExternalStore(
    subscribeVisibility, pageIsVisible, () => false,
  );

  useEffect(() => {
    if (!ref.current) return;
    return observe(ref.current, setOnScreen);
  }, []);

  return { cardRef: ref, workVisible: onScreen && pageVisible };
}
