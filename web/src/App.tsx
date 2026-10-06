import { NavLink, Route, Routes } from "react-router-dom";
import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { api } from "./api";
import LanguageSwitcher from "./components/LanguageSwitcher";
import Dashboard from "./pages/Dashboard";
import Agents from "./pages/Agents";
import AgentBuilder from "./pages/AgentBuilder";
import Playground from "./pages/Playground";
import Debugger from "./pages/Debugger";
import TestLab from "./pages/TestLab";
import Runs from "./pages/Runs";
import Events from "./pages/Events";
import Handlers from "./pages/Handlers";
import Tools from "./pages/Tools";
import Costs from "./pages/Costs";
import Settings from "./pages/Settings";

export default function App() {
  const { t } = useTranslation();
  const [ok, setOk] = useState(true);

  useEffect(() => {
    api
      .doctor()
      .then((d) => setOk(Boolean(d.runtime_ok) && !d.kill_switch))
      .catch(() => setOk(false));
  }, []);

  return (
    <div className="layout">
      <aside className="sidebar">
        <div className="brand">
          <div>
            Router<span>Ai</span>
          </div>
          <div
            className={`runtime-dot ${ok ? "" : "off"}`}
            title={ok ? t("common.runtimeOk") : t("common.runtimeIssue")}
          />
        </div>
        <nav className="nav">
          <NavLink to="/" end>
            {t("nav.dashboard")}
          </NavLink>
          <NavLink to="/agents">{t("nav.agents")}</NavLink>
          <NavLink to="/test-lab">{t("nav.testLab")}</NavLink>
          <NavLink to="/runs">{t("nav.runs")}</NavLink>
          <NavLink to="/events">{t("nav.events")}</NavLink>
          <NavLink to="/handlers">{t("nav.handlers")}</NavLink>
          <NavLink to="/tools">{t("nav.tools")}</NavLink>
          <NavLink to="/costs">{t("nav.costs")}</NavLink>
          <NavLink to="/settings">{t("nav.settings")}</NavLink>
        </nav>
        <LanguageSwitcher />
      </aside>
      <main className="main">
        <Routes>
          <Route path="/" element={<Dashboard />} />
          <Route path="/agents" element={<Agents />} />
          <Route path="/agents/:id" element={<AgentBuilder />} />
          <Route path="/agents/:id/playground" element={<Playground />} />
          <Route path="/agents/:id/debugger" element={<Debugger />} />
          <Route path="/test-lab" element={<TestLab />} />
          <Route path="/runs" element={<Runs />} />
          <Route path="/runs/:id" element={<Runs />} />
          <Route path="/events" element={<Events />} />
          <Route path="/handlers" element={<Handlers />} />
          <Route path="/tools" element={<Tools />} />
          <Route path="/costs" element={<Costs />} />
          <Route path="/settings" element={<Settings />} />
        </Routes>
      </main>
    </div>
  );
}
