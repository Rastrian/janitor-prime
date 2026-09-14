// Bundled java fixture corpus (E5 disposition): the fixed body of real-shape
// code the LangRegistry seeds its kind table from at first use. Parsed only.

package com.example.widgets;

import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.Optional;

/** A documented unit exercising classes, generics and lambdas. */
public final class Widget<T extends Comparable<T>> implements Comparable<Widget<T>> {
    public static final int MAX = 64;

    private final int id;
    private final T inner;
    private final Map<String, List<Integer>> cache = new HashMap<>();

    public Widget(int id, T inner) {
        this.id = id;
        this.inner = inner;
    }

    public Optional<String> classify() {
        if (id > 0 && id % 2 == 0) {
            return Optional.of("even");
        } else if (id < 0) {
            return Optional.empty();
        }
        return Optional.of("odd");
    }

    public int tally(Map<Integer, Integer> rows) {
        int acc = 0;
        for (var entry : rows.entrySet()) {
            acc += entry.getValue() * (entry.getKey() + 1);
        }
        while (acc > MAX) {
            acc /= 2;
        }
        return acc;
    }

    @Override
    public int compareTo(Widget<T> other) {
        return Integer.compare(this.id, other.id);
    }

    @Override
    public String toString() {
        return String.format("widget %d holding %s", id, inner);
    }

    public Runnable bound(int factor) {
        return () -> System.out.println(factor * id);
    }

    public String switchOver(int code) {
        return switch (code) {
            case 0, 1 -> "low";
            case 2 -> "mid";
            default -> {
                yield "high";
            }
        };
    }
}
