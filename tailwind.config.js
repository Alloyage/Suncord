/** @type {import('tailwindcss').Config} */
export default {
  content: ["./index.html", "./src/**/*.{ts,tsx}"],
  theme: {
    extend: {
      colors: {
        ink: "#0e0e0e",
        rail: "#111111",
        side: "#181818",
        panel: "#202020",
        line: "#2a2a2a",
        cream: "#f2f3f5",
        mute: "#949ba4",
        sun: "#ececec",
        leaf: "#bdbdbd",
        ember: "#d0d0d0",
      },
      fontFamily: {
        sans: ['ui-sans-serif', 'system-ui', '"Segoe UI"', 'sans-serif'],
      },
    },
  },
  plugins: [],
};
