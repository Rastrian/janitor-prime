import { normalizeCode } from "./format.js";

type WidgetProps = {
  raw: string;
};

export function CodeWidget({ raw }: WidgetProps) {
  const canonical = normalizeCode(raw);
  return (
    <section className="widget" data-code={canonical}>
      {/* the canonical token is the widget's whole payload */}
      <code>{canonical}</code>
    </section>
  );
}
