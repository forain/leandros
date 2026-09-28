/* icontrace.c — LD_PRELOAD diagnostic for ports/firefox: log every icon name
 * GTK / Firefox ask for, so the minimal icon theme in build-in-alpine.sh can
 * be derived from what is actually requested instead of guessed.
 *
 * Enabled by /bin/firefox when LEANDROS_FIREFOX_ICON_TRACE=1; each lookup
 * prints "ICONTRACE <entry> <name> [size]" on stderr.
 *
 * Coverage: libgtk-3 is linked -Bsymbolic, so GTK's calls to its OWN
 * gtk_icon_theme_* functions cannot be interposed. What can be: GTK's calls
 * into libgio's GThemedIcon constructors (every GtkImage/GtkIconHelper icon
 * name passes through one) and Firefox's direct gtk_icon_theme_* calls. Icons
 * named only in GTK's built-in CSS theme (-gtk-icontheme()) are covered by
 * build-in-alpine.sh extracting them from libgtk's resources instead. */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <stdio.h>

#define REAL(ret, name, ...) \
    static ret (*real)(__VA_ARGS__); \
    if (!real) real = (ret (*)(__VA_ARGS__))dlsym(RTLD_NEXT, name)

static void note(const char *entry, const char *name, int size) {
    if (!name) return;
    if (size >= 0) fprintf(stderr, "ICONTRACE %s %s %d\n", entry, name, size);
    else fprintf(stderr, "ICONTRACE %s %s\n", entry, name);
}

void *g_themed_icon_new(const char *name) {
    REAL(void *, "g_themed_icon_new", const char *);
    note("themed", name, -1);
    return real(name);
}

void *g_themed_icon_new_with_default_fallbacks(const char *name) {
    REAL(void *, "g_themed_icon_new_with_default_fallbacks", const char *);
    note("themed-fallbacks", name, -1);
    return real(name);
}

void *g_themed_icon_new_from_names(char **names, int len) {
    REAL(void *, "g_themed_icon_new_from_names", char **, int);
    for (int i = 0; names && (len < 0 ? names[i] != 0 : i < len); i++)
        note("themed-names", names[i], -1);
    return real(names, len);
}

void *gtk_icon_theme_lookup_icon(void *t, const char *name, int size, int flags) {
    REAL(void *, "gtk_icon_theme_lookup_icon", void *, const char *, int, int);
    note("lookup", name, size);
    return real(t, name, size, flags);
}

void *gtk_icon_theme_lookup_icon_for_scale(void *t, const char *name, int size, int scale, int flags) {
    REAL(void *, "gtk_icon_theme_lookup_icon_for_scale", void *, const char *, int, int, int);
    note("lookup-scale", name, size);
    return real(t, name, size, scale, flags);
}

void *gtk_icon_theme_load_icon(void *t, const char *name, int size, int flags, void **err) {
    REAL(void *, "gtk_icon_theme_load_icon", void *, const char *, int, int, void **);
    note("load", name, size);
    return real(t, name, size, flags, err);
}

void *gtk_icon_theme_load_icon_for_scale(void *t, const char *name, int size, int scale, int flags, void **err) {
    REAL(void *, "gtk_icon_theme_load_icon_for_scale", void *, const char *, int, int, int, void **);
    note("load-scale", name, size);
    return real(t, name, size, scale, flags, err);
}

void *gtk_icon_theme_choose_icon(void *t, const char **names, int size, int flags) {
    REAL(void *, "gtk_icon_theme_choose_icon", void *, const char **, int, int);
    for (int i = 0; names && names[i]; i++) note("choose", names[i], size);
    return real(t, names, size, flags);
}

void *gtk_icon_theme_choose_icon_for_scale(void *t, const char **names, int size, int scale, int flags) {
    REAL(void *, "gtk_icon_theme_choose_icon_for_scale", void *, const char **, int, int, int);
    for (int i = 0; names && names[i]; i++) note("choose-scale", names[i], size);
    return real(t, names, size, scale, flags);
}

int gtk_icon_theme_has_icon(void *t, const char *name) {
    REAL(int, "gtk_icon_theme_has_icon", void *, const char *);
    note("has", name, -1);
    return real(t, name);
}
