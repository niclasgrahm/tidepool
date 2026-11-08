//! The relationship diagram's geometry: what a table's card lists, where
//! the cards sit, and where the lines between them run.
//!
//! Pure functions over plain data, so `cargo test` reaches all of it; the
//! component in `pages.rs` only renders what these return. The routing is
//! Kayak's (`frontend/src/graph.rs` there): every line is axis-aligned, out
//! of one card along a stub, along a channel on a grid line, and into the
//! other, and channels that would lie on top of each other are moved apart.
//!
//! Two things differ from Kayak. A line leaves and arrives at the key
//! *rows* (`orders.customer_id` to `customers.customer_id`), not anywhere
//! along a face, so the card says which columns join and the line needn't.
//! And the layout is a tree on its side: fact tables on the right, each
//! table's one sides a column to its left, so selections (one side to many)
//! flow left to right.

use std::collections::{HashMap, HashSet};

use tidepool_protocol::{ModelInfo, RelationshipInfo, TableInfo};

/// Everything is measured in grid cells: cards, rows and channels.
pub const GRID: f64 = 20.0;
/// A card's width, 12 cells.
pub const WIDTH: f64 = 240.0;
/// The space between two columns of cards: room for a name on each stub and
/// a few channels between them.
const GAP: f64 = 240.0;
const MARGIN: f64 = 40.0;
/// A card's title, then its numbers; the first row's centre lands on a grid
/// line (`HEAD + STATS + ROW / 2` is 100), and so does every port.
const HEAD: f64 = 40.0;
const STATS: f64 = 50.0;
pub const ROW: f64 = 20.0;
/// How far a line runs straight out of a card before it turns.
const STUB: f64 = GRID;
/// The radius of a bend.
const CORNER: f64 = 8.0;
/// About how wide a character of a line's 12px name is, to tell whether the
/// name fits on its stub.
const CHAR_WIDTH: f64 = 6.6;

type Point = (f64, f64);
/// A channel: its x, and the y it runs from and to.
type Channel = (f64, f64, f64);

fn snap(value: f64) -> f64 {
    (value / GRID).round() * GRID
}

fn snap_up(value: f64) -> f64 {
    (value / GRID).ceil() * GRID
}

fn near(a: f64, b: f64) -> bool {
    (a - b).abs() < 0.01
}

/// What a row of a card is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowKind {
    /// Part of the primary key.
    Key,
    /// A foreign key, and the table it refers to.
    Refers(String),
    /// The event time.
    Time,
    /// A hierarchy over the table: its parent and key columns.
    Tree { parent: String, key: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// A column's name, or a hierarchy field's.
    pub name: String,
    pub kind: RowKind,
}

/// One table's card: the rows it lists and how tall it is.
#[derive(Clone, Debug, PartialEq)]
pub struct Card {
    pub table: String,
    pub rows: Vec<Row>,
    /// Columns not listed.
    pub more: usize,
    pub height: f64,
}

impl Card {
    /// Which row a column is on.
    fn row(&self, column: &str) -> Option<usize> {
        self.rows
            .iter()
            .position(|r| r.name == column && !matches!(r.kind, RowKind::Tree { .. }))
    }
}

/// The centre of row `i`, from the card's top.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn row_y(i: usize) -> f64 {
    HEAD + STATS + ROW / 2.0 + i as f64 * ROW
}

/// The cards: every table, listing its keys, its foreign keys, its event
/// time and its hierarchies, in column order. The other columns are counted.
#[must_use]
pub fn cards(tables: &[TableInfo], model: &ModelInfo) -> Vec<Card> {
    tables
        .iter()
        .map(|t| {
            let refers: HashMap<&str, &str> = model
                .relationships
                .iter()
                .filter(|r| r.from == t.name)
                .flat_map(|r| r.from_columns.iter().map(|c| (c.as_str(), r.to.as_str())))
                .collect();
            let mut rows: Vec<Row> = t
                .columns
                .iter()
                .filter_map(|c| {
                    let kind = if t.primary_key.contains(&c.name) {
                        RowKind::Key
                    } else if let Some(to) = refers.get(c.name.as_str()) {
                        RowKind::Refers((*to).to_string())
                    } else if t.event_time.as_ref() == Some(&c.name) {
                        RowKind::Time
                    } else {
                        return None;
                    };
                    Some(Row {
                        name: c.name.clone(),
                        kind,
                    })
                })
                .collect();
            let more = t.columns.len() - rows.len();
            rows.extend(
                model
                    .datasets
                    .iter()
                    .filter(|d| d.table == t.name)
                    .flat_map(|d| &d.fields)
                    .filter_map(|f| {
                        let h = f.hierarchy.as_ref()?;
                        Some(Row {
                            name: f.name.clone(),
                            kind: RowKind::Tree {
                                parent: h.parent.clone(),
                                key: h.key.clone(),
                            },
                        })
                    }),
            );
            #[allow(clippy::cast_precision_loss)]
            let listed = rows.len() as f64 + if more > 0 { 1.0 } else { 0.0 };
            // the footer takes a second line for the retention
            let footer = if t.retention.is_some() { 48.0 } else { 30.0 };
            Card {
                table: t.name.clone(),
                rows,
                more,
                height: snap_up(HEAD + STATS + listed * ROW + footer),
            }
        })
        .collect()
}

/// Where a card sits.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placed {
    pub x: f64,
    pub y: f64,
    pub height: f64,
}

/// A relationship's first columns: where its line attaches.
fn ends(r: &RelationshipInfo) -> (&str, &str) {
    (
        r.from_columns.first().map_or("", String::as_str),
        r.to_columns.first().map_or("", String::as_str),
    )
}

/// Where every card sits.
///
/// Columns: a table nobody refers to (a fact) is on the right, and every
/// table is a column left of the furthest-left table that refers to it, so
/// every line runs leftwards to its one side.
///
/// Rows: a tree. A table's children are the tables it refers to, in the
/// order of its own rows, so their lines don't cross; each subtree takes a
/// band of the page, and a table sits centred on its children's band, or
/// level with an only child, so that line is straight. Relationships are a
/// tree when selections flow both ways (the compiler checks); otherwise a
/// table two tables refer to is placed under the first, and the other line
/// is routed to it like any other.
#[must_use]
pub fn layout(cards: &[Card], relationships: &[RelationshipInfo]) -> HashMap<String, Placed> {
    let by_name: HashMap<&str, &Card> = cards.iter().map(|c| (c.table.as_str(), c)).collect();
    let mut depth: HashMap<&str, usize> = cards.iter().map(|c| (c.table.as_str(), 0)).collect();
    for _ in 0..cards.len() {
        for r in relationships {
            let next = depth.get(r.from.as_str()).copied().unwrap_or(0) + 1;
            if let Some(d) = depth.get_mut(r.to.as_str())
                && *d < next
            {
                *d = next;
            }
        }
    }
    let deepest = depth.values().copied().max().unwrap_or(0);

    let mut tree = Tree {
        cards: &by_name,
        relationships,
        claimed: HashSet::new(),
        y: HashMap::new(),
    };
    let mut top = MARGIN;
    let roots = cards
        .iter()
        .filter(|c| !relationships.iter().any(|r| r.to == c.table))
        .chain(cards.iter())
        .map(|c| c.table.as_str())
        .collect::<Vec<_>>();
    for root in roots {
        if tree.claimed.insert(root) {
            top = tree.place(root, top) + 3.0 * GRID;
        }
    }
    cards
        .iter()
        .map(|c| {
            #[allow(clippy::cast_precision_loss)]
            let column = (deepest - depth[c.table.as_str()]) as f64;
            let placed = Placed {
                x: MARGIN + column * (WIDTH + GAP),
                y: tree.y.get(c.table.as_str()).copied().unwrap_or(MARGIN),
                height: c.height,
            };
            (c.table.clone(), placed)
        })
        .collect()
}

struct Tree<'a> {
    cards: &'a HashMap<&'a str, &'a Card>,
    relationships: &'a [RelationshipInfo],
    claimed: HashSet<&'a str>,
    y: HashMap<&'a str, f64>,
}

impl<'a> Tree<'a> {
    /// Place `name` and its subtree from `top` down; returns the bottom.
    fn place(&mut self, name: &'a str, top: f64) -> f64 {
        let Some(card) = self.cards.get(name).copied() else {
            return top;
        };
        let mut kids: Vec<&'a RelationshipInfo> = self
            .relationships
            .iter()
            .filter(|r| r.from == name && self.cards.contains_key(r.to.as_str()))
            .collect();
        kids.sort_by_key(|r| card.row(ends(r).0));
        kids.retain(|r| self.claimed.insert(r.to.as_str()));
        if kids.is_empty() {
            let y = snap(top);
            self.y.insert(name, y);
            return y + card.height;
        }
        // An only child: pushed down far enough for its key row to be level
        // with ours, and we sit level with it, so its line is straight.
        let lone = match kids.as_slice() {
            [only] => {
                let (from, to) = ends(only);
                let kid = self.cards[only.to.as_str()];
                card.row(from)
                    .zip(kid.row(to))
                    .map(|(f, t)| row_y(f) - row_y(t))
            }
            _ => None,
        };
        let mut bottom = top;
        for (i, r) in kids.iter().enumerate() {
            let start = if i == 0 {
                top + lone.unwrap_or(0.0).max(0.0)
            } else {
                bottom + 2.0 * GRID
            };
            bottom = self.place(r.to.as_str(), start);
        }
        let want = match (lone, kids.first()) {
            (Some(offset), Some(only)) => self.y[only.to.as_str()] - offset,
            _ => f64::midpoint(top, bottom) - card.height / 2.0,
        };
        let y = snap(want.max(top));
        self.y.insert(name, y);
        bottom.max(y + card.height)
    }
}

/// One line, ready to draw.
#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    /// Its relationship's index in the model.
    pub index: usize,
    /// SVG path data, from the many side to the one side.
    pub path: String,
    /// The port on the many side's left face.
    pub from: (f64, f64),
    /// The port on the one side's right face.
    pub to: (f64, f64),
    /// Where the direction marks go, and which way the line runs there
    /// from the many side to the one side, in degrees.
    pub mark: (f64, f64, f64),
    /// Where the name goes, and whether it's anchored at its end.
    pub label: (f64, f64, bool),
}

/// The line from a port on the many side (leaving by its left face) to one
/// on the one side (arriving on its right face): out, along a channel `offset`
/// from half way (kept between the stubs), and in. Returns the corners and
/// the channel's (x, from y, to y) when there is one.
fn route(from: Point, to: Point, offset: f64) -> (Vec<Point>, Option<Channel>) {
    let a = (from.0 - STUB, from.1);
    let b = (to.0 + STUB, to.1);
    let (lo, hi) = (a.0.min(b.0), a.0.max(b.0));
    let x = snap(f64::midpoint(a.0, b.0) + offset).clamp(lo, hi);
    let points = simplify(vec![from, a, (x, a.1), (x, b.1), b, to]);
    let channel = (!near(a.1, b.1)).then_some((x, a.1, b.1));
    (points, channel)
}

/// Drop repeated points and corners a line runs straight through.
fn simplify(points: Vec<(f64, f64)>) -> Vec<(f64, f64)> {
    let mut out: Vec<(f64, f64)> = Vec::with_capacity(points.len());
    for p in points {
        if out.last().is_some_and(|l| near(l.0, p.0) && near(l.1, p.1)) {
            continue;
        }
        if let [.., a, b] = out.as_slice()
            && ((near(a.0, b.0) && near(b.0, p.0)) || (near(a.1, b.1) && near(b.1, p.1)))
        {
            out.pop();
        }
        out.push(p);
    }
    out
}

/// SVG path data through `points`, with the corners rounded, each by at most
/// half the shorter segment beside it so two close corners never overlap.
#[must_use]
pub fn rounded_path(points: &[(f64, f64)], radius: f64) -> String {
    use std::fmt::Write as _;
    let round = |v: f64| (v * 100.0).round() / 100.0;
    let distance = |p: (f64, f64), q: (f64, f64)| (q.0 - p.0).hypot(q.1 - p.1);
    let toward = |from: (f64, f64), to: (f64, f64), d: f64| {
        let length = distance(from, to);
        (
            from.0 + (to.0 - from.0) / length * d,
            from.1 + (to.1 - from.1) / length * d,
        )
    };
    let Some(first) = points.first() else {
        return String::new();
    };
    let mut path = format!("M {} {}", round(first.0), round(first.1));
    for w in points.windows(3) {
        let (before, corner, after) = (w[0], w[1], w[2]);
        let bend = radius
            .min(distance(before, corner) / 2.0)
            .min(distance(corner, after) / 2.0);
        let (enter, leave) = (toward(corner, before, bend), toward(corner, after, bend));
        let _ = write!(
            path,
            " L {} {} Q {} {} {} {}",
            round(enter.0),
            round(enter.1),
            round(corner.0),
            round(corner.1),
            round(leave.0),
            round(leave.1)
        );
    }
    if let Some(last) = points.get(1..).and_then(<[_]>::last) {
        let _ = write!(path, " L {} {}", round(last.0), round(last.1));
    }
    path
}

/// Every line between placed cards, in the model's order.
///
/// Channels between the same two columns all want the half-way line, so
/// each takes the nearest grid line to it (0, ±1, ±2 … cells away) that no
/// channel placed before it covers, top to bottom.
#[must_use]
pub fn lines(
    cards: &[Card],
    placed: &HashMap<String, Placed>,
    relationships: &[RelationshipInfo],
) -> Vec<Line> {
    let by_name: HashMap<&str, &Card> = cards.iter().map(|c| (c.table.as_str(), c)).collect();
    let mut wanted: Vec<(usize, Point, Point)> = relationships
        .iter()
        .enumerate()
        .filter_map(|(i, r)| {
            let (from_column, to_column) = ends(r);
            let (f, t) = (by_name.get(r.from.as_str())?, by_name.get(r.to.as_str())?);
            let (fp, tp) = (placed.get(&r.from)?, placed.get(&r.to)?);
            let from = (fp.x, fp.y + row_y(f.row(from_column)?));
            let to = (tp.x + WIDTH, tp.y + row_y(t.row(to_column)?));
            Some((i, from, to))
        })
        .collect();
    wanted.sort_by(|a, b| a.1.1.min(a.2.1).total_cmp(&b.1.1.min(b.2.1)));

    let mut taken: Vec<(f64, f64, f64)> = Vec::new();
    let mut out: Vec<Line> = wanted
        .into_iter()
        .map(|(index, from, to)| {
            let offsets = std::iter::once(0.0).chain((1..=6).flat_map(|s| {
                let d = f64::from(s) * GRID;
                [d, -d]
            }));
            let mut chosen = route(from, to, 0.0);
            for offset in offsets {
                let candidate = route(from, to, offset);
                let clear = candidate.1.is_none_or(|(x, y1, y2)| {
                    let (lo, hi) = (y1.min(y2), y1.max(y2));
                    !taken
                        .iter()
                        .any(|&(tx, tlo, thi)| near(tx, x) && lo < thi - 0.01 && tlo < hi - 0.01)
                });
                if clear {
                    chosen = candidate;
                    break;
                }
            }
            let (points, channel) = chosen;
            if let Some((x, y1, y2)) = channel {
                taken.push((x, y1.min(y2), y1.max(y2)));
            }
            let mark = match channel {
                Some((x, y1, y2)) => (x, f64::midpoint(y1, y2), if y2 > y1 { 90.0 } else { -90.0 }),
                None => (f64::midpoint(from.0, to.0), from.1, 180.0),
            };
            let name = &relationships[index].name;
            #[allow(clippy::cast_precision_loss)]
            let width = name.chars().count() as f64 * CHAR_WIDTH;
            let room = channel.map_or(from.0, |c| c.0) - (to.0 + 16.0) - 8.0;
            let label = if width <= room {
                (to.0 + 16.0, to.1 - 7.0, false)
            } else {
                (from.0 - 20.0, from.1 - 7.0, true)
            };
            Line {
                index,
                path: rounded_path(&points, CORNER),
                from,
                to,
                mark,
                label,
            }
        })
        .collect();
    out.sort_by_key(|l| l.index);
    out
}

/// How big the drawing is.
#[must_use]
pub fn extent(placed: &HashMap<String, Placed>) -> (f64, f64) {
    let width = placed.values().map(|p| p.x + WIDTH).fold(0.0, f64::max) + MARGIN;
    let height = placed.values().map(|p| p.y + p.height).fold(0.0, f64::max) + MARGIN;
    (width, height)
}

/// What selecting either side of a relationship does, in words.
#[must_use]
pub fn flow(r: &RelationshipInfo, both: bool) -> (String, String) {
    (
        format!("Selecting {} filters {}.", r.to, r.from),
        if both {
            format!(
                "Selecting {} also narrows {} to the rows it refers to.",
                r.from, r.to
            )
        } else {
            format!("Selecting {} leaves {} as it is.", r.from, r.to)
        },
    )
}

/// A sparkline's path data, `width` by `height`, scaled to its largest value.
#[must_use]
pub fn sparkline(values: &[f64], width: f64, height: f64) -> String {
    use std::fmt::Write as _;
    let max = values.iter().copied().fold(1.0, f64::max);
    #[allow(clippy::cast_precision_loss)]
    let step = width / (values.len().max(2) - 1) as f64;
    let mut d = String::new();
    for (i, v) in values.iter().enumerate() {
        #[allow(clippy::cast_precision_loss)]
        let x = i as f64 * step;
        let y = height - v / max * (height - 2.0) - 1.0;
        let _ = write!(d, "{}{x:.1} {y:.1}", if i == 0 { "M " } else { " L " });
    }
    d
}

#[cfg(test)]
mod tests {
    use tidepool_protocol::{ColumnInfo, DatasetInfo, FieldInfo, HierarchyInfo};

    use super::*;

    fn table(name: &str, mode: &str, key: &[&str], columns: &[&str]) -> TableInfo {
        TableInfo {
            name: name.into(),
            mode: mode.into(),
            primary_key: key.iter().map(|k| (*k).to_string()).collect(),
            columns: columns
                .iter()
                .map(|c| ColumnInfo {
                    name: (*c).into(),
                    column_type: "string".into(),
                    nullable: false,
                    encoding: "dict".into(),
                    filled: None,
                })
                .collect(),
            event_time: None,
            retention: None,
            rows: 0,
            written_total: 0,
            rows_per_second: 0.0,
            bytes: 0,
            segments: 0,
            epoch: 0,
        }
    }

    fn rel(name: &str, from: &str, column: &str, to: &str) -> RelationshipInfo {
        RelationshipInfo {
            name: name.into(),
            from: from.into(),
            from_columns: vec![column.into()],
            to: to.into(),
            to_columns: vec![column.into()],
            role: None,
            ai_context: None,
        }
    }

    fn model(relationships: Vec<RelationshipInfo>) -> ModelInfo {
        ModelInfo {
            relationships,
            filter_direction: "one_to_many".into(),
            ..ModelInfo::default()
        }
    }

    /// The retail example's tables and relationships.
    fn retail() -> (Vec<TableInfo>, ModelInfo) {
        let mut orders = table(
            "orders",
            "append",
            &[],
            &[
                "order_id",
                "customer_id",
                "product_id",
                "status",
                "ordered_at",
                "amount",
            ],
        );
        orders.event_time = Some("ordered_at".into());
        orders.retention = Some("730d".into());
        let tables = vec![
            table(
                "categories",
                "upsert",
                &["category"],
                &["category", "parent", "level"],
            ),
            table(
                "customers",
                "upsert",
                &["customer_id"],
                &["customer_id", "name", "region"],
            ),
            orders,
            table(
                "products",
                "upsert",
                &["product_id"],
                &["product_id", "name", "category"],
            ),
        ];
        let mut model = model(vec![
            rel("order_customer", "orders", "customer_id", "customers"),
            rel("order_product", "orders", "product_id", "products"),
            rel("product_category", "products", "category", "categories"),
        ]);
        model.datasets.push(DatasetInfo {
            name: "categories".into(),
            table: "categories".into(),
            description: None,
            ai_context: None,
            fields: vec![FieldInfo {
                name: "tree".into(),
                ty: "string".into(),
                expression: None,
                is_time: false,
                grains: Vec::new(),
                label: None,
                description: None,
                ai_context: None,
                hierarchy: Some(HierarchyInfo {
                    key: "category".into(),
                    parent: "parent".into(),
                    label: None,
                    kind: None,
                }),
            }],
        });
        (tables, model)
    }

    fn placed(tables: &[TableInfo], model: &ModelInfo) -> (Vec<Card>, HashMap<String, Placed>) {
        let cards = cards(tables, model);
        let placed = layout(&cards, &model.relationships);
        (cards, placed)
    }

    fn at<'a>(placed: &'a HashMap<String, Placed>, name: &str) -> &'a Placed {
        match placed.get(name) {
            Some(p) => p,
            None => panic!("{name} is placed"),
        }
    }

    #[test]
    fn a_card_lists_keys_foreign_keys_event_time_and_hierarchies() {
        let (tables, model) = retail();
        let cards = cards(&tables, &model);
        let orders = &cards[2];
        assert_eq!(
            orders.rows,
            vec![
                Row {
                    name: "customer_id".into(),
                    kind: RowKind::Refers("customers".into())
                },
                Row {
                    name: "product_id".into(),
                    kind: RowKind::Refers("products".into())
                },
                Row {
                    name: "ordered_at".into(),
                    kind: RowKind::Time
                },
            ]
        );
        assert_eq!(orders.more, 3);
        let categories = &cards[0];
        assert_eq!(categories.rows[0].kind, RowKind::Key);
        assert_eq!(
            categories.rows[1],
            Row {
                name: "tree".into(),
                kind: RowKind::Tree {
                    parent: "parent".into(),
                    key: "category".into()
                }
            }
        );
        assert!(
            cards.iter().all(|c| c.height % GRID == 0.0),
            "heights are whole cells"
        );
    }

    #[test]
    fn ports_sit_on_grid_lines() {
        assert_eq!(row_y(0) % GRID, 0.0);
        assert_eq!(row_y(3) % GRID, 0.0);
    }

    #[test]
    fn one_sides_sit_a_column_left_of_their_many_sides() {
        let (tables, model) = retail();
        let (_, placed) = placed(&tables, &model);
        let x = |t| at(&placed, t).x;
        assert!(x("orders") > x("customers"));
        assert_eq!(x("customers"), x("products"));
        assert!(x("products") > x("categories"));
    }

    #[test]
    fn children_are_stacked_in_the_order_of_their_rows() {
        let (tables, model) = retail();
        let (_, placed) = placed(&tables, &model);
        assert!(at(&placed, "customers").y < at(&placed, "products").y);
    }

    #[test]
    fn an_only_child_is_joined_by_a_straight_line() {
        let (tables, model) = retail();
        let (cards, placed) = placed(&tables, &model);
        let lines = lines(&cards, &placed, &model.relationships);
        let product_category = &lines[2];
        assert_eq!(product_category.from.1, product_category.to.1);
        assert_eq!(
            product_category.path.matches(" L ").count(),
            1,
            "{}",
            product_category.path
        );
    }

    #[test]
    fn cards_never_overlap() {
        // Northwind: a fact with two one sides, each with several of their own
        let tables = vec![
            table(
                "order_lines",
                "append",
                &[],
                &["order_id", "product_id", "amount"],
            ),
            table(
                "orders",
                "upsert",
                &["order_id"],
                &["order_id", "customer_id", "employee_id", "shipper_id"],
            ),
            table(
                "products",
                "upsert",
                &["product_id"],
                &["product_id", "supplier_id", "category_id"],
            ),
            table("customers", "upsert", &["customer_id"], &["customer_id"]),
            table("employees", "upsert", &["employee_id"], &["employee_id"]),
            table("shippers", "upsert", &["shipper_id"], &["shipper_id"]),
            table("categories", "upsert", &["category_id"], &["category_id"]),
            table("suppliers", "upsert", &["supplier_id"], &["supplier_id"]),
        ];
        let model = model(vec![
            rel("line_order", "order_lines", "order_id", "orders"),
            rel("line_product", "order_lines", "product_id", "products"),
            rel("product_category", "products", "category_id", "categories"),
            rel("product_supplier", "products", "supplier_id", "suppliers"),
            rel("order_customer", "orders", "customer_id", "customers"),
            rel("order_employee", "orders", "employee_id", "employees"),
            rel("order_shipper", "orders", "shipper_id", "shippers"),
        ]);
        let (cards, placed) = placed(&tables, &model);
        let all: Vec<&Placed> = placed.values().collect();
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                let apart = !near(a.x, b.x) || a.y + a.height <= b.y || b.y + b.height <= a.y;
                assert!(apart, "{a:?} overlaps {b:?}");
            }
        }
        // orders' one sides are above products', as orders is above products
        assert!(at(&placed, "shippers").y < at(&placed, "suppliers").y);
        // and no two channels lie on top of each other
        let lines = lines(&cards, &placed, &model.relationships);
        assert_eq!(lines.len(), 7);
        for line in &lines {
            assert!(
                line.path
                    .starts_with(&format!("M {} {}", line.from.0, line.from.1))
            );
        }
    }

    #[test]
    fn channels_that_would_overlap_are_moved_apart() {
        // two lines from one card to two cards, both needing the same
        // stretch of the half-way line
        let from = (480.0, 300.0);
        let first = route(from, (240.0, 100.0), 0.0).1;
        let second = route((480.0, 280.0), (240.0, 140.0), 0.0).1;
        assert_eq!(
            first.map(|c| c.0),
            second.map(|c| c.0),
            "both want the same line"
        );
        let tables = vec![
            table("f", "append", &[], &["a", "b"]),
            table("a", "upsert", &["a"], &["a"]),
            table("b", "upsert", &["b"], &["b"]),
        ];
        let model = model(vec![rel("fa", "f", "a", "a"), rel("fb", "f", "b", "b")]);
        let (cards, mut placed) = placed(&tables, &model);
        // force both one sides above the fact, so their channels share a stretch
        if let Some(p) = placed.get_mut("a") {
            p.y = 0.0;
        }
        if let Some(p) = placed.get_mut("b") {
            p.y = 20.0;
        }
        if let Some(p) = placed.get_mut("f") {
            p.y = 400.0;
        }
        let lines = lines(&cards, &placed, &model.relationships);
        let channel_x = |l: &Line| l.mark.0;
        assert_ne!(channel_x(&lines[0]), channel_x(&lines[1]));
    }

    #[test]
    fn corners_are_rounded_but_never_past_half_a_segment() {
        let d = rounded_path(&[(0.0, 0.0), (10.0, 0.0), (10.0, 4.0)], 8.0);
        assert_eq!(d, "M 0 0 L 8 0 Q 10 0 10 2 L 10 4");
        assert_eq!(rounded_path(&[(0.0, 0.0), (5.0, 0.0)], 8.0), "M 0 0 L 5 0");
        assert_eq!(rounded_path(&[], 8.0), "");
    }

    #[test]
    fn a_long_name_moves_to_the_many_side() {
        let tables = vec![
            table("f", "append", &[], &["a"]),
            table("a", "upsert", &["a"], &["a"]),
        ];
        let model = model(vec![rel(&"x".repeat(40), "f", "a", "a")]);
        let (cards, placed) = placed(&tables, &model);
        let line = &lines(&cards, &placed, &model.relationships)[0];
        assert!(line.label.2, "anchored at its end, beside the many side");
        let short = model_with_name("fa");
        let (cards, placed) = super::tests::placed(&tables, &short);
        assert!(!lines(&cards, &placed, &short.relationships)[0].label.2);
    }

    fn model_with_name(name: &str) -> ModelInfo {
        model(vec![rel(name, "f", "a", "a")])
    }

    #[test]
    fn the_flow_says_what_a_selection_does() {
        let r = rel("order_customer", "orders", "customer_id", "customers");
        assert_eq!(flow(&r, false).0, "Selecting customers filters orders.");
        assert_eq!(
            flow(&r, false).1,
            "Selecting orders leaves customers as it is."
        );
        assert_eq!(
            flow(&r, true).1,
            "Selecting orders also narrows customers to the rows it refers to."
        );
    }

    #[test]
    fn a_sparkline_spans_its_box() {
        assert_eq!(sparkline(&[0.0, 2.0], 10.0, 10.0), "M 0.0 9.0 L 10.0 1.0");
    }
}
