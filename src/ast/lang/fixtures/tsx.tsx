// Bundled tsx fixture corpus (E5 disposition, F9b): the fixed body of
// real-shape code the LangRegistry seeds its kind table from at first
// use. Parsed only; never executed. Exercises the JSX surface the plain
// typescript grammar cannot parse (measured: JSX under
// LANGUAGE_TYPESCRIPT yields ERROR nodes — the reason tsx is its own
// registry entry).

import { type ReactElement } from "react";

/** A row of badges with an HTML comment parked between JSX siblings. */
export function Badges({ labels }: { labels: string[] }): ReactElement {
  return (
    <ul className="badges">
      {labels.map((label) => (
        <li key={label}>
          {/* badge cells below are grouped */}
          {label}
        </li>
      ))}
    </ul>
  );
}

type PanelProps = {
  title: string;
  children: ReactElement;
};

export const Panel = ({ title, children }: PanelProps): ReactElement => (
  <section data-title={title.toLowerCase()}>
    {/* section body keeps a banner comment above the children slot */}
    {children}
  </section>
);
