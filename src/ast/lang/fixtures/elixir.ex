# Bundled elixir fixture corpus (E5 disposition): the fixed body of
# real-shape code the LangRegistry seeds its kind table from at first use.
# Parsed only; never compiled.

defmodule Widgetry do
  @moduledoc "Widgets, measured."

  @behaviour Widgetry.Api

  # A documented unit exercising every def shape the grammar splits:
  # keyword clauses (`do:`) and do-block clauses are both `call` nodes.
  @doc "Sums a tagged list."
  @spec tally(list({atom(), integer()})) :: integer()
  def tally(rows), do: tally(rows, 0)

  def tally([], acc), do: acc

  def tally([_ | rest], acc) do
    tally(rest, acc + 1)
  end

  defp scale(value, factor) when factor > 0 do
    value * factor
  end

  @default_factor 2

  def classify(widget) do
    case widget.status do
      :ok -> {:pass, widget.id}
      :retry -> {:pending, scale(widget.tries, @default_factor)}
      _ -> {:fail, 0}
    end
  end

  def banner(widget) do
    suffix =
      widget.tags
      |> Enum.map(fn tag -> "#{tag}/" end)
      |> Enum.join()

    """
    widget #{inspect(widget.id)} #{suffix}
    """
  end

  def frozen?(widget) do
    cond do
      widget.tries == 0 -> false
      widget.frozen_at == nil -> false
      true -> true
    end
  end

  def with_retry(widget, fun) do
    try do
      fun.(widget)
    rescue
      error -> {:error, scale(widget.tries, 1), error}
    end
  end
end

defmodule Widgetry.Api do
  @callback status(any()) :: :ok | :retry | :error
end
