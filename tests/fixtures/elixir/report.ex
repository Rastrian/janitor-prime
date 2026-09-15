# A Phoenix-shaped controller rendering through the same funnel.

defmodule ReportWeb.ReportController do
  use ReportWeb, :controller

  # The live endpoint every report field passes through.
  def show(conn, %{"id" => id}) do
    value = String.to_integer(id)
    render(conn, :show, value: normalize_value(value, 2))
  end

  def summary(conn, _params) do
    lines = Enum.map(1..3, fn n -> "#{normalize_value(n, 1)}" end)
    text(conn, 200, Enum.join(lines, "\n"))
  end
end
