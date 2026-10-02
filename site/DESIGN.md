---
name: Pilus Marketing Site
description: An atmospheric software-factory showcase for the native Pilus client.
colors:
  signal-amber: "#e99a24"
  signal-pale: "#f5dfb6"
  paper: "#f7f6f2"
  white: "#ffffff"
  ink: "#10100f"
  night: "#0c0c0b"
  night-raised: "#171716"
  muted: "#6f6b66"
  line: "rgba(16, 16, 15, 0.13)"
  line-dark: "rgba(255, 255, 255, 0.12)"
typography:
  display:
    fontFamily: "Geist, -apple-system, BlinkMacSystemFont, Helvetica Neue, sans-serif"
    fontSize: "clamp(3.8rem, 7vw, 6.25rem)"
    fontWeight: 690
    lineHeight: 0.93
    letterSpacing: "-0.04em"
  headline:
    fontFamily: "Geist, -apple-system, BlinkMacSystemFont, Helvetica Neue, sans-serif"
    fontSize: "clamp(2.8rem, 5vw, 4.8rem)"
    fontWeight: 680
    lineHeight: 0.94
    letterSpacing: "-0.04em"
  body:
    fontFamily: "Geist, -apple-system, BlinkMacSystemFont, Helvetica Neue, sans-serif"
    fontSize: "16px"
    fontWeight: 400
    lineHeight: 1.5
  label:
    fontFamily: "Geist Mono, ui-monospace, SFMono-Regular, monospace"
    fontSize: "12px"
    fontWeight: 500
    lineHeight: 1.5
    letterSpacing: "0.08em"
rounded:
  image: "10px"
  inset: "14px"
  card: "18px"
  panel: "24px"
  stage: "26px"
  pill: "999px"
spacing:
  xs: "7px"
  sm: "11px"
  md: "17px"
  lg: "28px"
  xl: "68px"
components:
  button-primary:
    backgroundColor: "{colors.night}"
    textColor: "{colors.white}"
    typography: "{typography.body}"
    rounded: "{rounded.pill}"
    padding: "0 21px"
    height: "46px"
  button-secondary:
    backgroundColor: "{colors.white}"
    textColor: "{colors.ink}"
    typography: "{typography.body}"
    rounded: "{rounded.pill}"
    padding: "0 21px"
    height: "46px"
---

# Design System: Pilus Marketing Site

## Overview

**Creative North Star: "Warm Software Factory"**

The Pilus marketing site presents a serious native tool with the calm confidence of editorial product photography. A generated sky-and-landscape field runs from the navigation through the hero actions, then fades to white before a clipped real interface capture keeps the product—not decorative chrome—at the center.

The system is intentionally sparse. Scale, contrast, and vertical pacing create drama; amber is reserved for live signals and compact metadata. The product story is specific: Pi + Iroh + Luvus become one native software-factory workspace. This system applies to the static marketing site only and does not replace the native app's GPUI conventions.

**Key Characteristics:**
- Oversized, blunt Geist headlines with tight leading.
- An atmospheric landscape and faint grid spanning navigation, headline, and hero actions.
- The primary Pilus screenshot clipped onto white immediately after the atmospheric fade, without fake chrome.
- Warm neutral fields separated by hairline rules.
- Rounded controls and containers without ornamental gradients.
- One coordinated entrance sequence with a reduced-motion fallback.

## Colors

The palette is warm, nearly monochrome, and punctuated by a single operational amber.

### Primary
- **Signal Amber:** Used sparingly for status, focus, bullets, prompts, and metadata that communicates activity.

### Neutral
- **Warm Paper:** The default long-form page field.
- **Clear White:** Hero and control surfaces that need maximum contrast.
- **Pilus Ink:** Primary copy and light-surface controls.
- **Proof Black:** Product stages and high-contrast closing panels.
- **Raised Night:** Subtle layering within dark surfaces.
- **Quiet Graphite:** Secondary copy that must remain comfortably readable.
- **Light and Dark Hairlines:** Structure grids and framed product captures without heavy chrome.

**The Signal Rarity Rule.** Amber marks activity or attention; it never becomes a large decorative field.

## Typography

**Display Font:** Geist with native sans-serif fallbacks  
**Body Font:** Geist with native sans-serif fallbacks  
**Label/Mono Font:** Geist Mono with system monospace fallbacks

**Character:** Geist keeps the page contemporary and direct. Tight, heavy display settings carry the cinematic scale; the mono face is reserved for operational labels, status, and source-oriented details.

### Hierarchy
- **Display** (690, `clamp(3.8rem, 7vw, 6.25rem)`, 0.93): Three-line hero statements.
- **Headline** (680, `clamp(2.8rem, 5vw, 4.8rem)`, 0.94): Major section and closing statements.
- **Title** (640–660, `clamp(1.4rem, 3.2vw, 3rem)`, 0.98–1.02): Feature and inventory titles.
- **Body** (400, 16–18px, 1.5): Product explanation, kept to readable measures.
- **Label** (500–650, 12px, 0.08em when uppercase): Status, metadata, and compact navigational context.

**The Compression Rule.** Display lines stay tightly led and tightly tracked; body copy restores normal rhythm and breathing room.

## Layout

The site uses a centered fluid container capped at 1180px with a 20px minimum viewport gutter. The hero places its centered promise directly inside an atmospheric full-bleed field, fades to white, then introduces the clipped product capture; later sections alternate between a split architecture stack, compact screenshot cards, and ruled capability inventories.

Vertical rhythm is intentionally generous, commonly ranging from 100px to 180px between major ideas. At 900px, multi-column proof, feature, closing, FAQ, and footer layouts collapse to one column. At 680px, type and padding compress, buttons become full width where useful, and product frames preserve their rounded silhouette without causing horizontal overflow.

**The Proof-After-Promise Rule.** Large claims are followed by a real product capture before the next narrative section.

## Elevation & Depth

Depth is tonal first and shadowed only where a focal object should lift from the page. The Pilus screenshot remains unframed and derives depth from its placement against the landscape. The software-factory stack relies on layered near-black fills, staggered slabs, and hairline rules; ordinary cards remain flat.

### Shadow Vocabulary
- **Mark Lift** (`0 18px 46px rgba(16, 16, 15, 0.19)`): The inline Pilus icon in the hero.

**The Tonal-First Rule.** Prefer field changes, clipping, and hairlines to shadows; reserve shadow for the small first-view mark.

## Shapes

Pills define actions. Product imagery uses 12–18px clipped top corners, cards use 18px, and major panels use 24px. The Pilus icon retains its softly squared native shape. Borders remain one-pixel hairlines.

## Components

### Buttons
- **Shape:** Fully rounded pill with a 46px minimum height.
- **Primary:** Proof-black fill, white text, 21px horizontal padding, and semibold Geist.
- **Secondary:** White fill, ink text, and a quiet hairline border.
- **Hover / Focus:** Tonal fill shift, 180ms transition, and a 3px amber focus-visible outline.

### Cards / Containers
- **Corner Style:** 18px for feature cards and 24px for major dark panels.
- **Background:** Warm gray for explanatory cards; proof black for the factory formula and closing panel.
- **Screenshot treatment:** Compact 16:9 crops in bento cards, with the hero capture clipped onto the white field immediately after the landscape.
- **Shadow Strategy:** Flat by default.
- **Border:** Hairlines organize ruled content; product captures are not placed inside simulated frames.

### Navigation
- **Style:** A 78px transparent header with a left brand, centered 13px links, and a compact download action.
- **Hover / Focus:** Links reveal an underline; keyboard focus uses the global amber outline.
- **Mobile:** Center links disappear below 900px while the brand and source action remain.

### Atmospheric Hero
- **Style:** A subtle white grid dissolves into a generated sky and terrain scene. A real Pilus screenshot enters as one clipped surface with no caption bar, status row, border, or simulated window frame.
- **Motion:** The screenshot arrives once with restrained vertical movement and clipping. Reduced-motion mode removes the transition.

### Software Factory Stack
- **Style:** A split architecture section pairs three ruled narrative rows—Pi, Iroh, and Luvus—with a dark isometric line-art system diagram.
- **Diagram:** Pi and Iroh rise as distinct block systems from a shared Luvus foundation, while a central Pilus control plane joins both sides.
- **Hierarchy:** Pi is the agent harness, Iroh is peer-to-peer connectivity, Luvus is workspace persistence, and Pilus is the native control surface.
- **Mobile:** The diagram leads, followed by the three full-width narrative rows.

### FAQ Rows
- **Style:** Native `details` and `summary` behavior with hairline separators and a circular plus/minus affordance.
- **State:** Expanded content keeps the same field and introduces no floating overlay.

## Do's and Don'ts

### Do:
- **Do** use real Pilus captures as the primary proof material.
- **Do** maintain AA-readable secondary text on warm fields.
- **Do** preserve the coordinated hero-copy and clipped-product reveal.
- **Do** keep amber limited to signals, focus, prompts, and small metadata.
- **Do** keep the Pi + Iroh + Luvus relationship explicit and factual.

### Don't:
- **Don't** add release claims beyond the intentionally dead download and wishlist destinations.
- **Don't** invent metrics, testimonials, customers, benchmarks, or unsupported build requirements.
- **Don't** add gradient text, ornamental glows, or multiple independent scroll reveals.
- **Don't** apply this marketing system to the native GPUI application.
