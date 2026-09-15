# Entries with tags, counted and labelled.

defmodule Catalog do
  @moduledoc "Entries with tags."

  @unit :each
  @doc "Counts tagged entries."
  def tagged_count(entries) do
    entries
    |> Enum.filter(&match?(%{tags: [_ | _]}, &1))
    |> Enum.count()
  end

  # An adjacent pair behind a comment.
  def label(entry) do
    "#{@unit}: #{entry.name}"
  end

  defp render(entry) do
    label(entry)
  end
end
