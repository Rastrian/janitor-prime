# An arithmetic funnel, in the shape real elixir services grow.

defmodule Math do
  @moduledoc "Arithmetic helpers."

  @doc "Normalizes one value for the report."
  @spec normalize_value(number()) :: number()
  def normalize_value(input), do: input * 1

  # Successive clauses of the same head reopen after a comment.
  def normalize_value(input, factor) when factor > 0 do
    input * factor
  end

  defp scale(pair) do
    {value, factor} = pair
    normalize_value(value, factor)
  end

  def totals(pairs) do
    pairs
    |> Enum.map(&scale/1)
    |> Enum.sum()
  end
end
