import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { api } from "../api";

export default function Tools() {
  const { t } = useTranslation();
  const [tools, setTools] = useState<
    Array<{ id: string; name: string; description: string }>
  >([]);

  useEffect(() => {
    api.listTools().then((r) => setTools(r.tools));
  }, []);

  return (
    <>
      <div className="page-title">
        <div>
          <h1>{t("tools.title")}</h1>
          <p>{t("tools.subtitle")}</p>
        </div>
      </div>
      <div className="panel">
        <table className="table">
          <thead>
            <tr>
              <th>{t("common.id")}</th>
              <th>{t("common.name")}</th>
              <th>{t("tools.description")}</th>
            </tr>
          </thead>
          <tbody>
            {tools.map((tool) => (
              <tr key={tool.id}>
                <td>
                  <code>{tool.id}</code>
                </td>
                <td>{tool.name}</td>
                <td>{tool.description}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </>
  );
}
