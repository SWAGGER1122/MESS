/** @type {import('tailwindcss').Config} */
export default {
  content: ["./index.html", "./src/**/*.{js,ts,jsx,tsx}"],
  darkMode: "class",
  theme: {
    extend: {
      colors: {
        tg: {
          bg: "#0e1621",
          sidebar: "#17212b",
          hover: "#202b36",
          active: "#2b5278",
          accent: "#5288c1",
          outgoing: "#2b5278",
          incoming: "#182533",
          border: "#101921",
          text: "#f5f5f5",
          muted: "#708499",
          emerald: "#10b981",
        },
      },
      backdropBlur: {
        xs: "2px",
      },
    },
  },
  plugins: [],
};
