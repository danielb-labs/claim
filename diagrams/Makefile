# Rebuild all diagrams from their .d2 sources.
# Needs: d2 (brew install d2), rsvg-convert (brew install librsvg)

DIAGRAMS := architecture lifecycle graph-propagation drift-path

all: $(DIAGRAMS:%=%.png)

%.svg: %.d2
	d2 --layout=elk $< $@

%.png: %.svg
	rsvg-convert -z 2 $< -o $@

clean:
	rm -f *.svg *.png

.PHONY: all clean
