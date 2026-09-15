# KP-6-elixir curated pattern 3: attribute chains above a def.
# `@moduledoc`/`@doc`/`@spec` are `unary_operator` siblings stacking
# directly over the def `call` — the attribute chain is a seq of
# unary_operators ending in a call, and no comment sits inside this
# chain, so both modes agree on the count.
defmodule Kp6.Attr do
  @moduledoc "Attr chain."

  @doc "Adds two integers."
  @spec add(integer(), integer()) :: integer()
  def add(a, b), do: a + b
end
