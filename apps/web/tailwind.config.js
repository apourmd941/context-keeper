/** @type {import('tailwindcss').Config} */
export default {
  content: ['./index.html', './src/**/*.{ts,tsx}'],
  theme: {
    extend: {
      fontFamily: {
        sans: ['"Plus Jakarta Sans"', 'system-ui', 'sans-serif'],
        mono: ['"JetBrains Mono"', 'ui-monospace', 'monospace'],
      },
      colors: {
        // Neutral foundation remapped onto the shared Selran theme palette
        // (selran-theme.css) so context-keeper matches the rest of the suite.
        // Ordering (light 50 → dark 950) is preserved so every existing
        // bg-/text-/border-ink-* class keeps its role; layout is unchanged.
        ink: {
          // Role-based scale resolved from CSS variables (selran-theme.css)
          // so dark AND light themes drive every existing ink-* class.
          50: 'rgb(var(--ink-50) / <alpha-value>)',
          100: 'rgb(var(--ink-100) / <alpha-value>)',
          200: 'rgb(var(--ink-200) / <alpha-value>)',
          300: 'rgb(var(--ink-300) / <alpha-value>)',
          500: 'rgb(var(--ink-500) / <alpha-value>)',
          700: 'rgb(var(--ink-700) / <alpha-value>)',
          800: 'rgb(var(--ink-800) / <alpha-value>)',
          900: 'rgb(var(--ink-900) / <alpha-value>)',
          950: 'rgb(var(--ink-950) / <alpha-value>)',
        },
        accent: {
          DEFAULT: 'rgb(var(--accent-rgb) / <alpha-value>)',
          contrast: 'var(--accent-contrast)',
        },
        // Tier colors — bright pastel pops over the warm dark backdrop.
        project: {
          DEFAULT: '#fb923c', // tangerine
          soft: '#fbbf24',
          accent: '#fde68a',
        },
        topic: {
          DEFAULT: '#a78bfa', // periwinkle
          soft: '#c4b5fd',
          accent: '#ddd6fe',
        },
        session: {
          DEFAULT: '#34d399', // mint
          soft: '#6ee7b7',
          accent: '#a7f3d0',
        },
      },
      boxShadow: {
        card: '0 1px 2px rgba(8, 14, 24, 0.18), 0 8px 24px -12px rgba(8, 14, 24, 0.35)',
        overlay: '0 12px 40px -8px rgba(8, 14, 24, 0.45)',
      },
      keyframes: {
        dash: {
          to: { strokeDashoffset: '-12' },
        },
      },
      animation: {
        dash: 'dash 1.2s linear infinite',
      },
    },
  },
  plugins: [],
};
