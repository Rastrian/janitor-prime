# Bundled ruby fixture corpus (E5 disposition): the fixed body of
# real-shape code the LangRegistry seeds its kind table from at first use.
# Parsed only; never executed.

module Widgetry
  # A documented unit exercising modules, classes and methods.
  class Widget
    attr_reader :id
    attr_accessor :tags

    def initialize(id = 0, tags = [])
      @id = id
      @tags = tags
    end

    def classify
      if id.even? && id.positive?
        "even"
      elsif id.negative?
        nil
      else
        "odd"
      end
    end

    def tally(rows)
      acc = rows.sum { |key, value| value * (key + 1) }
      acc /= 2 while acc > 64
      acc
    end

    def self.build(id)
      new(id, [])
    end
  end
end

def comprehend(items)
  squares = items.filter_map { |n| n.zero? ? nil : n * n }
  lookup = squares.each_with_object({}) { |n, h| h[n] = :"#{n}" }
  [squares.sum, lookup.length]
end

banner = <<~TEXT
  widgetry #{comprehend([1, 2, 3])}
TEXT

puts banner, Widgetry::Widget.build(4).classify
