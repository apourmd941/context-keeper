import { useEffect, useState } from "react";

/**
 * Selran launch splash (shared design — mirrors the Reviewer app).
 *
 * Shows the app's simple logo centered over a soft halo, holds briefly, then
 * shrinks it and slides it into the top-left corner before fading out. Drop it in
 * once at the entry point alongside <App/>; it self-animates and removes itself,
 * so it needs no wiring into the app's internals.
 *
 * Per app, only `color` / `label` / `tagline` change.
 */

// Where the logo lands (top-left corner) and its final size.
const TARGET_X = 14;
const TARGET_Y = 10;
const TARGET_SIZE = 40;
// Centered splash logo size.
const SPLASH_SIZE = 460;

interface SimpleLogoProps {
  size: number | string;
  from: string;
  to: string;
  label: string;
}

// The "new simple logo": a rounded square in the app's brand color with the
// app name + the "selran" wordmark — same shape language as the Reviewer logo.
function SimpleLogo({ size, from, to, label }: SimpleLogoProps) {
  return (
    <svg viewBox="0 0 144 144" xmlns="http://www.w3.org/2000/svg" style={{ width: size, height: size }}>
      <defs>
        <linearGradient id="splashGrad" x1="0%" y1="0%" x2="100%" y2="100%">
          <stop offset="0%" stopColor={from} />
          <stop offset="100%" stopColor={to} />
        </linearGradient>
        <filter id="splashShadow">
          <feDropShadow dx="0" dy="2" stdDeviation="3" floodColor="#000" floodOpacity="0.2" />
        </filter>
      </defs>
      <rect x="12" y="12" width="120" height="120" rx="24" ry="24" fill="url(#splashGrad)" filter="url(#splashShadow)" />
      <text x="72" y="64" fontFamily="system-ui, -apple-system, sans-serif" fontSize="30" fontWeight="700" fill="white" textAnchor="middle" letterSpacing="-0.5">
        {label}
      </text>
      <text x="72" y="97" fontFamily="system-ui, -apple-system, sans-serif" fontSize="13" fontWeight="500" fill="white" textAnchor="middle" letterSpacing="1.5" opacity="0.9">
        selran
      </text>
    </svg>
  );
}

type Phase = "hold" | "shrink" | "fade" | "gone";

interface AppSplashProps {
  from?: string;
  to?: string;
  label?: string;
  tagline?: string;
  halo?: string;
}

export default function AppSplash({
  from = "#f43f5e",
  to = "#e11d48",
  label = "Context",
  tagline = "CONTEXT KEEPER",
  halo = "rgba(225, 29, 72, 0.16)",
}: AppSplashProps) {
  // Phases: hold → shrink → fade → gone
  const [phase, setPhase] = useState<Phase>("hold");

  useEffect(() => {
    const t1 = setTimeout(() => setPhase("shrink"), 750);
    const t2 = setTimeout(() => setPhase("fade"), 750 + 600);
    const t3 = setTimeout(() => setPhase("gone"), 750 + 600 + 240);
    return () => {
      clearTimeout(t1);
      clearTimeout(t2);
      clearTimeout(t3);
    };
  }, []);

  if (phase === "gone") return null;

  const shrinking = phase === "shrink" || phase === "fade";
  const fading = phase === "fade";
  const ease = "cubic-bezier(0.4, 0, 0.2, 1)";

  return (
    <div
      style={{
        position: "fixed",
        inset: 0,
        zIndex: 2147483647, // max — nothing in the app can paint over the splash
        backgroundColor: "var(--bg)", // shared Selran --bg (theme-aware)
        display: "flex",
        alignItems: "center",
        justifyContent: "center",
        opacity: fading ? 0 : 1,
        transition: "opacity 0.24s ease-out",
        pointerEvents: fading ? "none" : "auto",
      }}
    >
      {/* Soft halo behind the logo — fades as the logo shrinks. */}
      <div
        style={{
          position: "fixed",
          left: "50%",
          top: "50%",
          transform: "translate(-50%, -50%)",
          width: `${SPLASH_SIZE * 2.8}px`,
          height: `${SPLASH_SIZE * 2.8}px`,
          borderRadius: "50%",
          background: `radial-gradient(circle, ${halo} 0%, transparent 70%)`,
          opacity: shrinking ? 0 : 1,
          transition: `opacity 0.3s ${ease}`,
          pointerEvents: "none",
        }}
      />

      {/* Logo — animates from center to the top-left corner. */}
      <div
        style={{
          position: "fixed",
          left: shrinking ? `${TARGET_X}px` : "50%",
          top: shrinking ? `${TARGET_Y}px` : "50%",
          width: shrinking ? `${TARGET_SIZE}px` : `${SPLASH_SIZE}px`,
          height: shrinking ? `${TARGET_SIZE}px` : `${SPLASH_SIZE}px`,
          transform: shrinking ? "translate(0, 0)" : "translate(-50%, -50%)",
          transition: shrinking
            ? `left 0.6s ${ease}, top 0.6s ${ease}, width 0.6s ${ease}, height 0.6s ${ease}, transform 0.6s ${ease}`
            : "none",
          zIndex: 100000,
        }}
      >
        <SimpleLogo size="100%" from={from} to={to} label={label} />
      </div>

      {/* Wordmark below the logo — fades out as the logo shrinks. */}
      <div
        style={{
          position: "fixed",
          left: "50%",
          top: `calc(50% + ${SPLASH_SIZE / 2 + 36}px)`,
          transform: "translateX(-50%)",
          display: "flex",
          flexDirection: "column",
          alignItems: "center",
          gap: "8px",
          opacity: shrinking ? 0 : 1,
          transition: "opacity 0.25s ease-out",
          pointerEvents: "none",
          fontFamily: "system-ui, -apple-system, sans-serif",
        }}
      >
        <span style={{ fontSize: "44px", fontWeight: 700, color: "var(--fg)", letterSpacing: "-0.5px" }}>
          Selran
        </span>
        {tagline ? (
          <span style={{ fontSize: "13px", color: "var(--fg-subtle)", fontWeight: 400, letterSpacing: "1.5px" }}>
            {tagline}
          </span>
        ) : null}
      </div>
    </div>
  );
}
