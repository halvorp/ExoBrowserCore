PKGS   = wpe-webkit-1.1 wpebackend-fdo-1.0 wayland-server glib-2.0 gobject-2.0

CFLAGS  ?= -O2 -g -Wall -Wextra -Wno-unused-parameter -fPIC
CFLAGS  += $(shell pkg-config --cflags $(PKGS))
LDLIBS  += $(shell pkg-config --libs   $(PKGS))

all: libgutted_wpe.a gutted-host

libgutted_wpe.a: gutted_wpe.o
	$(AR) rcs $@ $^

gutted_wpe.o: gutted_wpe.c gutted_wpe.h
	$(CC) $(CFLAGS) -c -o $@ $<

main.o: main.c gutted_wpe.h
	$(CC) $(CFLAGS) -c -o $@ $<

gutted-host: main.o libgutted_wpe.a
	$(CC) $(CFLAGS) -o $@ main.o -L. -lgutted_wpe $(LDLIBS)

clean:
	rm -f gutted-host libgutted_wpe.a *.o

.PHONY: all clean
