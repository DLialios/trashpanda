//! Which depots to download, by Steam's own rule. The developer tags each
//! depot in the app's record with who it is for, and Steam downloads every
//! depot whose tags all fit the install: its system, architecture and
//! language, whether it wants low violence, and which DLC it has. A depot
//! without tags is for everyone. A kind of tag this program does not know is
//! offered too, under the name Steam gives it, so that nothing a record says
//! about a depot is passed over.

use std::collections::{BTreeMap, BTreeSet};

use crate::appinfo::Record;
use crate::depot::Depot;
use crate::glimpse::{self, Platform};

/// The tags on a depot. Each one left out means "for everyone".
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tags {
    /// The systems it is for (`oslist`); it fits any one of them.
    pub systems: Vec<Platform>,
    /// `osarch`: "32" or "64".
    pub arch: Option<String>,
    /// A Steam language code, such as "german" or "koreana".
    pub language: Option<String>,
    /// Only for installs that want low violence.
    pub low_violence: bool,
    /// The DLC app it belongs to (`dlcappid` or `optionaldlc`).
    pub dlc: Option<u32>,
    /// Tags of kinds this program does not know, as Steam wrote them.
    pub other: BTreeMap<String, String>,
}

impl Tags {
    /// The tags in the words the selectors use; `dlc` names a DLC.
    pub fn words(&self, dlc: impl Fn(u32) -> String) -> Vec<String> {
        let mut words: Vec<String> = self.systems.iter().map(|system| system.name().to_string()).collect();
        words.extend(self.arch.as_deref().map(arch_name));
        words.extend(self.language.as_deref().map(|code| language_name(code).to_string()));
        words.extend(self.low_violence.then(|| "Low violence".to_string()));
        words.extend(self.other.iter().map(|(kind, value)| format!("{kind}: {value}")));
        words.extend(self.dlc.map(dlc));
        words
    }
}

/// The tags of each depot: from the app's record if there is one, where a
/// depot it does not list has none. Without a record, just the systems a
/// depot has programs or libraries for, worked out from its files.
pub fn tags(depots: &[Depot], record: Option<&Record>) -> BTreeMap<u32, Tags> {
    let tags = |depot: &Depot| match record {
        Some(record) => record.depots.get(&depot.id).cloned().unwrap_or_default(),
        None => Tags { systems: glimpse::systems(depot), ..Tags::default() },
    };
    depots.iter().map(|depot| (depot.id, tags(depot))).collect()
}

/// A kind of tag that can be chosen.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Selector {
    System,
    Arch,
    Language,
    LowViolence,
    /// A kind this program does not know, by Steam's name for it.
    Other(String),
    Dlc,
}

impl Selector {
    pub fn label(&self) -> &str {
        match self {
            Selector::System => "System",
            Selector::Arch => "Architecture",
            Selector::Language => "Language",
            Selector::LowViolence => "Low violence",
            Selector::Other(kind) => kind,
            Selector::Dlc => "DLC",
        }
    }
}

/// The values the depots' tags take, each kind in the order it is offered.
#[derive(Debug, Default, PartialEq)]
pub struct Offer {
    /// Windows, macOS, Linux.
    pub systems: Vec<Platform>,
    pub archs: Vec<String>,
    /// In the order of the first depot with each.
    pub languages: Vec<String>,
    pub low_violence: bool,
    /// The values of each kind this program does not know.
    pub other: BTreeMap<String, Vec<String>>,
    /// In ID order.
    pub dlc: Vec<u32>,
}

impl Offer {
    pub fn of(tags: &BTreeMap<u32, Tags>) -> Self {
        let mut offer = Offer::default();
        fn add<T: PartialEq + Clone>(values: &mut Vec<T>, value: &T) {
            if !values.contains(value) {
                values.push(value.clone());
            }
        }
        for tags in tags.values() {
            tags.systems.iter().for_each(|system| add(&mut offer.systems, system));
            tags.arch.iter().for_each(|arch| add(&mut offer.archs, arch));
            tags.language.iter().for_each(|language| add(&mut offer.languages, language));
            tags.dlc.iter().for_each(|dlc| add(&mut offer.dlc, dlc));
            offer.low_violence |= tags.low_violence;
            for (kind, value) in &tags.other {
                add(offer.other.entry(kind.clone()).or_default(), value);
            }
        }
        offer.other.values_mut().for_each(|values| values.sort());
        offer.systems.sort();
        offer.archs.sort();
        offer.dlc.sort();
        offer
    }

    /// The kinds of tag there is a choice in, in the order they are shown. A
    /// kind with a single value and no "none" to choose instead, such as the
    /// one system a package is for, is not shown, though it still applies.
    pub fn selectors(&self) -> Vec<Selector> {
        let known = [Selector::System, Selector::Arch, Selector::Language, Selector::LowViolence];
        let other = self.other.keys().cloned().map(Selector::Other);
        let kinds = known.into_iter().chain(other).filter(|selector| self.count(selector) > 1);
        kinds.chain((!self.dlc.is_empty()).then_some(Selector::Dlc)).collect()
    }

    /// How many values a kind offers to choose from, counting "No extra" or
    /// "None" where it has one; for DLC, how many DLC there are.
    pub fn count(&self, selector: &Selector) -> usize {
        match selector {
            Selector::Dlc => self.dlc.len(),
            _ => self.values(selector, &Choices::default()).len(),
        }
    }

    /// The values a kind other than DLC can be set to, in the order they are
    /// offered, each with `choices` changed to it.
    pub fn values(&self, selector: &Selector, choices: &Choices) -> Vec<Value> {
        let value = |name: &str, none: bool, set: &dyn Fn(&mut Choices)| {
            let mut changed = choices.clone();
            set(&mut changed);
            Value { name: name.into(), none, chosen: changed == *choices, choices: changed }
        };
        // Where "No extra" or "None" can be chosen, it comes first.
        let none = |name: &str, set: &dyn Fn(&mut Choices)| std::iter::once(value(name, true, set));
        match selector {
            Selector::System => self.systems.iter()
                .map(|&system| value(system.name(), false, &|choices| choices.system = Some(system))).collect(),
            Selector::Arch => self.archs.iter()
                .map(|arch| value(&arch_name(arch), false, &|choices| choices.arch = Some(arch.clone()))).collect(),
            Selector::Language => none("No extra", &|choices| choices.language = None)
                .chain(self.languages.iter().map(|code| {
                    value(language_name(code), false, &|choices| choices.language = Some(code.clone()))
                })).collect(),
            Selector::LowViolence => [(false, "Off"), (true, "On")].into_iter().take(if self.low_violence { 2 } else { 1 })
                .map(|(on, name)| value(name, false, &|choices| choices.low_violence = on)).collect(),
            // The values of a kind this program does not know, as Steam wrote them.
            Selector::Other(kind) => none("None", &|choices| _ = choices.other.remove(kind))
                .chain(self.other.get(kind).into_iter().flatten().map(|other| {
                    value(other, false, &|choices| _ = choices.other.insert(kind.clone(), other.clone()))
                })).collect(),
            Selector::Dlc => Vec::new(),
        }
    }

    /// What is chosen at first: the system `here` if it is offered, else the
    /// first; 64-bit; English if there is one, else no extra language; no
    /// low violence; and every DLC. What a kind this program does not know
    /// means can only be guessed from its values: with one, it is taken for
    /// a switch like low violence and starts off; with several, for a choice
    /// like the system, and starts on the first.
    pub fn start(&self, here: Option<Platform>) -> Choices {
        let several = self.other.iter().filter(|(_, values)| values.len() > 1);
        Choices {
            system: here.filter(|here| self.systems.contains(here)).or(self.systems.first().copied()),
            arch: self.archs.iter().find(|arch| *arch == "64").or(self.archs.first()).cloned(),
            language: self.languages.iter().find(|language| *language == "english").cloned(),
            low_violence: false,
            other: several.map(|(kind, values)| (kind.clone(), values[0].clone())).collect(),
            without: BTreeSet::new(),
        }
    }
}

/// One value of a kind of tag.
#[derive(Clone, Debug, PartialEq)]
pub struct Value {
    pub name: String,
    /// Whether it is "No extra" or "None": no value of the kind at all.
    pub none: bool,
    /// Whether it is the value chosen now.
    pub chosen: bool,
    /// The choices with this value chosen.
    pub choices: Choices,
}

/// What is chosen for each kind of tag.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Choices {
    pub system: Option<Platform>,
    pub arch: Option<String>,
    /// None for no extra language: only the depots without one.
    pub language: Option<String>,
    pub low_violence: bool,
    /// The value chosen for each kind this program does not know; a kind
    /// left out has none chosen, which leaves out the depots tagged with it.
    pub other: BTreeMap<String, String>,
    /// The DLC left out.
    pub without: BTreeSet<u32>,
}

/// Whether a depot's tags fit the choices, its DLC aside.
fn fits(tags: &Tags, choices: &Choices) -> bool {
    (tags.systems.is_empty() || choices.system.is_some_and(|system| tags.systems.contains(&system)))
        && tags.arch.as_ref().is_none_or(|arch| choices.arch.as_ref() == Some(arch))
        && tags.language.as_ref().is_none_or(|language| choices.language.as_ref() == Some(language))
        && (!tags.low_violence || choices.low_violence)
        && tags.other.iter().all(|(kind, value)| choices.other.get(kind) == Some(value))
}

/// Steam's rule: a depot is chosen when every tag it has fits.
pub fn chosen(tags: &Tags, choices: &Choices) -> bool {
    fits(tags, choices) && tags.dlc.is_none_or(|dlc| !choices.without.contains(&dlc))
}

/// The depots the choices leave out.
pub fn skip(tags: &BTreeMap<u32, Tags>, choices: &Choices) -> BTreeSet<u32> {
    tags.iter().filter(|(_, tags)| !chosen(tags, choices)).map(|(id, _)| *id).collect()
}

fn total(tags: &BTreeMap<u32, Tags>, sizes: &BTreeMap<u32, u64>, keep: impl Fn(&Tags) -> bool) -> u64 {
    tags.iter().filter(|(_, tags)| keep(tags)).map(|(id, _)| sizes.get(id).copied().unwrap_or(0)).sum()
}

/// What ticking `dlc` adds under the other choices.
pub fn dlc_size(tags: &BTreeMap<u32, Tags>, sizes: &BTreeMap<u32, u64>, choices: &Choices, dlc: u32) -> u64 {
    total(tags, sizes, |tags| tags.dlc == Some(dlc) && fits(tags, choices))
}

/// What choosing a value of `selector`, as `choices` has it, adds under the
/// other choices: the depots chosen that would not be with no value of that
/// kind chosen at all.
pub fn value_size(tags: &BTreeMap<u32, Tags>, sizes: &BTreeMap<u32, u64>, choices: &Choices, selector: &Selector) -> u64 {
    let mut none = choices.clone();
    match selector {
        Selector::System => none.system = None,
        Selector::Arch => none.arch = None,
        Selector::Language => none.language = None,
        Selector::LowViolence => none.low_violence = false,
        Selector::Other(kind) => _ = none.other.remove(kind),
        Selector::Dlc => none.without.extend(tags.values().filter_map(|tags| tags.dlc)),
    }
    total(tags, sizes, |tags| chosen(tags, choices) && !chosen(tags, &none))
}

pub fn arch_name(arch: &str) -> String {
    format!("{arch}-bit")
}

/// The name of a Steam language code, from Steam's own list; a code not in
/// it as Steam wrote it.
pub fn language_name(code: &str) -> &str {
    match code {
        "english" => "English",
        "german" => "German",
        "french" => "French",
        "italian" => "Italian",
        "koreana" => "Korean",
        "spanish" => "Spanish (Spain)",
        "latam" => "Spanish (Latin America)",
        "schinese" => "Chinese (Simplified)",
        "tchinese" => "Chinese (Traditional)",
        "russian" => "Russian",
        "thai" => "Thai",
        "japanese" => "Japanese",
        "portuguese" => "Portuguese",
        "brazilian" => "Portuguese (Brazil)",
        "polish" => "Polish",
        "danish" => "Danish",
        "dutch" => "Dutch",
        "finnish" => "Finnish",
        "norwegian" => "Norwegian",
        "swedish" => "Swedish",
        "hungarian" => "Hungarian",
        "czech" => "Czech",
        "romanian" => "Romanian",
        "turkish" => "Turkish",
        "arabic" => "Arabic",
        "bulgarian" => "Bulgarian",
        "greek" => "Greek",
        "ukrainian" => "Ukrainian",
        "vietnamese" => "Vietnamese",
        "indonesian" => "Indonesian",
        _ => code,
    }
}
