import { useTranslation } from "react-i18next";
import { AppLang, setAppLang } from "../i18n";

export default function LanguageSwitcher() {
  const { t, i18n } = useTranslation();
  const current: AppLang = i18n.language.startsWith("ru") ? "ru" : "en";

  return (
    <div className="lang-switch" role="group" aria-label={t("nav.language")}>
      <button
        type="button"
        className={current === "en" ? "active" : ""}
        onClick={() => setAppLang("en")}
      >
        {t("nav.langEn")}
      </button>
      <button
        type="button"
        className={current === "ru" ? "active" : ""}
        onClick={() => setAppLang("ru")}
      >
        {t("nav.langRu")}
      </button>
    </div>
  );
}
