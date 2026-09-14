/* Bundled c fixture corpus (E5 disposition): the fixed body of real-shape
   code the LangRegistry seeds its kind table from at first use. Parsed
   only; never compiled. */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define WIDGET_MAX 64

typedef enum Kind { ALPHA, BETA, GAMMA } Kind;

typedef struct Widget {
    unsigned id;
    char tag[16];
    struct Widget *next;
} Widget;

typedef int (*tally_fn)(const int *rows, size_t len);

static Kind classify(unsigned id) {
    if (id == 0) return ALPHA;
    if (id % 2 == 0) return BETA;
    return GAMMA;
}

int tally(const int *rows, size_t len) {
    int acc = 0;
    for (size_t i = 0; i < len; i++) {
        acc += rows[i] * (int)(i + 1);
    }
    while (acc > WIDGET_MAX) {
        acc /= 2;
    }
    return acc;
}

char *describe(const Widget *w) {
    char *out = malloc(sizeof(char) * 32);
    if (out == NULL) {
        return NULL;
    }
    snprintf(out, 32, "widget %u [%s]", w->id, w->tag);
    return out;
}

int main(int argc, char **argv) {
    int rows[] = {1, 2, 3};
    Widget w = {7, "alpha", NULL};
    tally_fn fn = &tally;
    Kind k = classify(w.id);
    char *text = describe(&w);
    if (argc > 1 && strcmp(argv[1], "-v") == 0) {
        printf("%s kind=%d total=%d\n", text, k, fn(rows, sizeof rows / sizeof rows[0]));
    }
    free(text);
    return k == GAMMA ? EXIT_SUCCESS : EXIT_FAILURE;
}
