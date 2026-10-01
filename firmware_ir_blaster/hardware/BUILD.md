# Building the IR add-on board (soldered)

> **Verified 2026-10-01:** the first board built from these steps passed
> every check and works (sending and receiving).

Step by step, from loose parts to a working blaster. The circuit is the one
verified on the breadboard ([README.md](README.md), sections 1-3); only its
home changes: a **6 x 28 hole perfboard** (separate copper ring per hole),
joined to the ESP32 board by 5 wires.

**Keep the drawing open while you work:** [images/perfboard.svg](images/perfboard.svg)

**Hole names are the ones printed on the board**: the long side is
lettered **A ... Z** and then **A' B'** (the print starts again at A and B:
those last two are written A' and B' here), the short side is numbered
**01 ... 06**. A hole is a letter and a number: **C04** = letter C, number
04. They're the same holes from both sides, so you can always find one by
its print.

- **Top half of the drawing: the PART SIDE.** Hold the board so the print
  reads normally with **A at the left and 06 at the top**: that side is
  where the parts go. The IR LEDs point out of the **A end** (left), the
  receiver out of the **B' end** (right). The resistors lie flat along the
  board, their legs 4 holes apart (e.g. B01 and F01).
- **Bottom half: the COPPER SIDE**, the board turned over its long edge
  (top to bottom): letters still run A to B' left to right, but now **01 is
  at the top**. This is where you solder.

Tick each box as you go. **USB unplugged the whole time** until step 19.

---

## Before you start

### Tools and material

- [ ] Soldering iron (about 330 °C if it's adjustable) with a wet sponge or brass wool
- [ ] Solder with flux inside (0.6-1 mm)
- [ ] Side cutters, small pliers
- [ ] Multimeter
- [ ] The perfboard (6 x 28 holes)
- [ ] The parts from the breadboard: 2 IR LEDs, 2N2222, 2x 47 Ω, 1 kΩ, 10 kΩ, 100 Ω, 100 µF, 100 nF, CHQ1838
- [ ] ~8 cm of **bare solid wire** for the GND line: e.g. one wire from an old network cable with the insulation pulled off (solid copper)
- [ ] 5 jumper wires with a **female** end (one end goes onto the ESP32's pins, the other is cut off and soldered to the board). Different colours help: red, blue/black, orange, green, yellow/white

### Safety

- The iron's tip is ~330 °C: always put it back in its stand.
- Solder in a room with fresh air, don't breathe the smoke.
- Wash your hands afterwards (solder may contain lead).
- Wear glasses when you cut legs: the cut ends fly.

### How to solder one joint

1. Wipe the tip on the sponge, then put a tiny bit of solder on the tip (it should look shiny).
2. Touch the tip to **both** the copper ring and the leg at the same time, 1-2 seconds.
3. Feed solder into the joint (not onto the tip) until it flows around the leg.
4. Take the solder away first, then the iron.
5. Don't move the leg for 2-3 seconds. A good joint is a small shiny cone. A ball sitting on top, or a dull grey blob, needs reheating.

### How to make a bridge (join two neighbouring holes)

Lay a short **cut-off leg** across the two copper rings on the copper side,
solder it to both rings, cut off what sticks out. Easier and more reliable
than pulling a blob of solder across.

### Check the parts before soldering

They come off the breadboard, so mixing them up is easy. Use the meter:

- [ ] Resistors on the ohm range: **47 Ω** (two of them), **1 kΩ**, **10 kΩ**, **100 Ω**. Write the value on a bit of tape on each if it helps.
- [ ] Both clear 5 mm parts are **IR LEDs**, not photodiodes: `3V3` -> 100 Ω -> part (long leg first) -> `GND` on the breadboard, then measure across the part: **1.2-1.3 V** = IR LED (0.6-0.7 V = photodiode).

---

## Parts (lowest first, so the board lies flat while you solder)

**Resistors lie flat**: bend both legs down 90° right at the ends of the
body, so the legs are **4 holes apart** (about 10 mm, e.g. B01 and F01),
and push it in until the body touches the board.

Push each part in from the part side, bend its legs outward a little on
the copper side so it doesn't fall out, turn the board over, solder.
**Don't cut legs yet** (step 11).

- [ ] **1. R1a, 47 Ω**: legs into **B01** and **F01**.
- [ ] **2. R1b, 47 Ω**: legs into **B02** and **F02** (right next to R1a).
- [ ] **3. R3, 10 kΩ**: legs into **C04** and **G04**.
- [ ] **4. R2, 1 kΩ**: legs into **C05** and **G05** (right next to R3).
- [ ] **5. R4, 100 Ω**: legs into **W05** and **A'05**.
- [ ] **6. C2, 100 nF** (no polarity, either way round): legs into **Z03** and **Z04**.
- [ ] **7. Q1, 2N2222**: hold it with the flat face toward you, legs down: left to right **E, B, C**. Legs: **E into B03, B into B04, C into B05**; its **flat face then points toward the B' end** (away from the LEDs). Leave ~5 mm of leg above the board and solder each leg quickly (2 s): transistors don't like long heat.
- [ ] **8. C1, 100 µF**: **long leg (+) into G02**, the **striped side (−) into G03**. Backwards it can pop: check twice.
- [ ] **9. U1, CHQ1838 receiver**: hold it with the lens toward you, legs down: left to right **OUT, GND, VCC**. Bend all three legs 90° about 3 mm below the body, so it lies flat with the lens pointing **out of the B' end** (away from the board). Legs: **OUT into B'02, GND into B'03, VCC into B'04**.
- [ ] **10. D1 and D2, the IR LEDs**: bend both legs of each LED 90° about 5 mm below the body, so the LED lies flat and points **out of the A end**. **D1:** long leg (+) into **A01**, short leg into **A02**. **D2:** long leg (+) into **A03**, short leg into **A04**. The flat edge on the LED's rim is the short-leg side.

- [ ] **11. Cut the legs** on the copper side, about 1 mm above each joint. **Keep the cut-off pieces**: they become the bridges.

---

## Joins on the copper side

Use the **bottom half of the drawing** (copper side, 01 at the top). Grey = bridge.
Each line: the two holes, and what it joins.

- [ ] **12. The 17 bridges**, one at a time:
  - [ ] A01 - B01: LED 1 (+) to R1a
  - [ ] B01 - B02: R1a to R1b (the two 47 Ω side by side)
  - [ ] F01 - F02: the other ends of R1a and R1b (5V)
  - [ ] F01 - G01: 5V to the 5Vin wire's hole
  - [ ] F02 - G02: 5V to C1 (+)
  - [ ] A02 - A03: LED 1 (−) to LED 2 (+)
  - [ ] A04 - A05: LED 2 (−) to A05 (an empty hole, a stepping stone)
  - [ ] A05 - B05: A05 to Q1's collector (so LED 2 (−) reaches the collector)
  - [ ] B04 - C04: Q1's base to R3
  - [ ] C04 - C05: R3 to R2 (base)
  - [ ] G04 - G03: R3's other end to GND (G03 is on the GND line)
  - [ ] G05 - H05: R2 to the GPIO4 wire's hole
  - [ ] B'02 - A'02: receiver OUT to the GPIO5 wire's hole
  - [ ] B'04 - A'04: receiver VCC to A'04 (an empty hole where 3 bridges meet)
  - [ ] A'04 - Z04: A'04 to C2
  - [ ] A'04 - A'05: A'04 to R4
  - [ ] V05 - W05: R4 to the 3V3 wire's hole

  **Not joined, although they're neighbours** (a stray blob there is the most common mistake):
  - the legs of one part: B03-B04-B05 (Q1), G02-G03 (C1), Z03-Z04 (C2), B'02-B'03-B'04 (receiver), A01-A02 and A03-A04 (LEDs)
  - **A03-B03, A04-B04, B05-C05, B02-B03**: different parts of the circuit side by side near the transistor
  - **F02-F03, G04-G05, A'02-A'03, A'03-A'04**: next to the GND line

- [ ] **13. The GND line**: the bare solid wire along the **03 holes**, on the copper side, from **B03 to B'03**. Lay it straight over the rings of line 03, touching neither line 02 nor line 04, and solder it at **B03, G03, Z03, B'03** and every 4-5 holes in between to hold it flat. I03 gets the GND wire later.

---

## Check before any power

- [ ] **14. Look**: in good light (a phone's camera zoomed in works as a magnifier), compare every joint and bridge with the drawing. No solder between holes that aren't meant to be joined.

- [ ] **15. Beeper (continuity), should BEEP:**
  - [ ] G01 - F01, G01 - F02, G01 - G02 (5V)
  - [ ] A01 - B02 (LED 1 + to both 47 Ω)
  - [ ] A02 - A03, A04 - B05
  - [ ] B04 - C05, G05 - H05
  - [ ] I03 - B03, I03 - G04, I03 - Z03, I03 - B'03 (GND)
  - [ ] B'04 - Z04, B'04 - A'05, V05 - W05, B'02 - A'02

- [ ] **16. Beeper, must NOT beep** (a beep here = a short, find it before going on):
  - [ ] G01 - I03 (5V to GND)
  - [ ] V05 - I03 (3V3 to GND)
  - [ ] G01 - V05 (5V to 3V3)
  - [ ] H05 - I03 and A'02 - I03 (the GPIOs to GND)

- [ ] **17. Ohm range:**
  - [ ] B01 - F01: about **23 Ω** (the two 47 Ω together; some meters beep here, that's fine)
  - [ ] W05 - A'05: about **100 Ω**
  - [ ] C05 - G05: about **1 kΩ**
  - [ ] C04 - G04: up to **10 kΩ** (less is fine: the transistor sits in parallel)

---

## Wires and first power-up

- [ ] **18. The 5 wires.** Cut one end off each jumper wire (keep the female end long enough to reach the ESP32), strip 3 mm, push it through from the part side, solder on the copper side:

  | Hole | Wire | ESP32 pin (as printed on the board) |
  |---|---|---|
  | G01 | red | `5Vin` (the IN-OUT pads must stay bridged) |
  | I03 | blue or black | `GND` |
  | V05 | orange | `3V3` |
  | H05 | green | `4` |
  | A'02 | yellow or white | `5` |

  ⚠️ **3V3 and 5Vin must not be swapped**: the receiver on 5V would put 5 V on GPIO5.

- [ ] **19. Power up**: plug the ESP32's **COM** port into USB. Nothing should get warm (touch the transistor and the receiver after 10 s).
  - [ ] G01 to I03: about **4.4-4.6 V**
  - [ ] V05 to I03: about **3.3 V**

- [ ] **20. LED test**: `make monitor-ir`, then `ledtest 3`. A phone's front camera shows the LEDs glowing purple. During the test, D1's long leg (A01) to GND reads about **2.9 V**.

- [ ] **21. Send test**: point the LEDs at the LED strip (`nec 0x00 0x40` = Power) or the LG TV (`nec 0x04 0x08`), 1-2 m away.

- [ ] **22. Receive test**: point a remote at the receiver's lens (from close by), press a button once, then type `last` in the console: it shows the code it heard (the LED strip's Power: NEC address 0x00, command 0x40). `replay` sends it back out.

Done: the board is the blaster's IR part. Later, a case: the LEDs looking
out of the A end, the receiver's lens out of the B' end.

---

The layout is made and checked by [tools/perfboard.py](tools/perfboard.py)
(every net joins exactly the legs it should; `--list` prints every hole).
If the layout changes, change it there and rerun it, then update this list.
