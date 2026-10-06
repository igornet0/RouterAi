import i18n from "i18next";
import { initReactI18next } from "react-i18next";
import en from "./locales/en.json";
import ru from "./locales/ru.json";

export const SUPPORTED_LANGS = ["en", "ru"] as const;
export type AppLang = (typeof SUPPORTED_LANGS)[number];

const STORAGE_KEY = "routerai.lang";

function detectLang(): AppLang {
  const saved = localStorage.getItem(STORAGE_KEY);
  if (saved === "en" || saved === "ru") return saved;
  const nav = navigator.language.toLowerCase();
  if (nav.startsWith("ru")) return "ru";
  return "en";
}

void i18n.use(initReactI18next).init({
  resources: {
    en: { translation: en },
    ru: { translation: ru },
  },
  lng: detectLang(),
  fallbackLng: "en",
  interpolation: { escapeValue: false },
});

i18n.on("languageChanged", (lng) => {
  const lang = lng === "ru" ? "ru" : "en";
  localStorage.setItem(STORAGE_KEY, lang);
  document.documentElement.lang = lang;
});

document.documentElement.lang = i18n.language === "ru" ? "ru" : "en";

export function setAppLang(lang: AppLang) {
  void i18n.changeLanguage(lang);
}

export default i18n;
