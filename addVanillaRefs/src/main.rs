use tes3::esp::{
    AiPackage, Dialogue, DialogueInfo, DialogueType2, EditorId, EffectId, MagicEffect, NpcFlags,
    ObjectInfo, Plugin, TES3Object, TypeInfo,
};

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

const VANILLA_PLUGIN_NAMES: [&str; 3] = ["Bloodmoon.esm", "Tribunal.esm", "Morrowind.esm"];

fn main() -> std::io::Result<()> {
    let mut plugin = Plugin::from_path("Starwind.esp")?;

    let base_plugins = VANILLA_PLUGIN_NAMES
        .into_iter()
        .map(Plugin::from_path)
        .collect::<std::io::Result<Vec<_>>>()?;
    let mut defined_ids = collect_defined_ids(&plugin);
    let mut defined_effects: HashSet<_> = plugin
        .objects_of_type::<MagicEffect>()
        .map(|effect| effect.effect_id)
        .collect();
    // Do not let dirty vanilla DialogueInfo records bootstrap their speakers
    // into the actor population used by dialogue reconstruction.
    add_vanilla_refs_without_dialogue(
        &mut plugin,
        &base_plugins,
        &mut defined_ids,
        &mut defined_effects,
        "pre-dialogue",
    );
    for object in &mut plugin.objects {
        let ai_packages = match object {
            TES3Object::Creature(creature) => &mut creature.ai_packages,
            TES3Object::Npc(npc) => &mut npc.ai_packages,
            _ => continue,
        };
        for ai_package in ai_packages {
            let cell = match ai_package {
                AiPackage::Escort(pkg) => &mut pkg.cell,
                AiPackage::Follow(pkg) => &mut pkg.cell,
                _ => continue,
            };
            if !defined_ids.contains(&cell.to_ascii_lowercase()) {
                *cell = "igtestcell".to_string();
            }
        }
    }
    print_dialogue_liveness_population(&plugin);

    decouple_dialogue_infos(&mut plugin, &base_plugins);
    // Dialogue materialization can introduce references from retained INFOs.
    // Resolve those references before removing the masters from the header.
    add_vanilla_refs(
        &mut plugin,
        &base_plugins,
        &mut defined_ids,
        &mut defined_effects,
        "post-dialogue",
    );

    strip_deleted_records(&mut plugin);
    remove_exact_duplicate_magic_effects(&mut plugin);

    // Remove masters from the plugin header.
    let header = plugin.header_mut().unwrap();
    header.masters.clear();
    header.version = 1.3;

    // Save the updated plugin.
    plugin.save_path("Starwind.esp")?;

    Ok(())
}

#[derive(Default)]
struct DialogueGroup {
    dialogue: Dialogue,
    dialogue_type: DialogueType2,
    infos: Vec<DialogueInfo>,
}

impl DialogueGroup {
    fn insert_info(&mut self, info: DialogueInfo) {
        let existing_index = self
            .infos
            .iter()
            .position(|existing| same_id(&existing.id, &info.id));
        if let Some(index) = existing_index {
            if same_id(&self.infos[index].prev_id, &info.prev_id) {
                self.infos[index] = info;
                return;
            }
        }

        // OpenMW computes the insertion point before moving an existing node.
        // This matters when prev_id refers to that same node.
        let before_index = if info.prev_id.is_empty() {
            0
        } else {
            self.infos
                .iter()
                .position(|existing| same_id(&existing.id, &info.prev_id))
                .map_or(self.infos.len(), |index| index + 1)
        };
        if let Some(index) = existing_index {
            let insertion_index = before_index - usize::from(index < before_index);
            self.infos.remove(index);
            self.infos.insert(insertion_index, info);
        } else {
            self.infos.insert(before_index, info);
        }
    }
}

fn same_id(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

type DialogueRecords = HashMap<String, DialogueGroup>;

fn decouple_dialogue_infos(plugin: &mut Plugin, base_plugins: &[Plugin]) {
    let effective_records = collect_effective_dialogue_records(plugin, base_plugins);
    let starwind_ids = collect_dialogue_ids(plugin);
    let population = collect_dialogue_population(plugin);
    let live_topics = collect_live_dialogue_topics(plugin, &effective_records, &starwind_ids);
    let (live_infos, mut audit) =
        collect_live_dialogue_infos(&effective_records, &starwind_ids, &live_topics, &population);

    let mut rebuilt = Vec::with_capacity(plugin.objects.len());
    let mut emitted_topics = HashSet::new();
    for object in plugin.objects.drain(..) {
        match object {
            TES3Object::Dialogue(dialogue) => {
                let topic = dialogue.id.to_ascii_lowercase();
                if emitted_topics.contains(&topic) {
                    continue;
                }
                let Some(group) = effective_records.get(&topic) else {
                    continue;
                };
                rebuilt.push(TES3Object::Dialogue(dialogue));
                if let Some(live_ids) = live_infos.get(&topic) {
                    append_live_dialogue_infos(&mut rebuilt, group, live_ids);
                }
                emitted_topics.insert(topic);
            }
            TES3Object::DialogueInfo(_) => {}
            object => rebuilt.push(object),
        }
    }
    let mut missing_topics: Vec<_> = live_topics
        .keys()
        .filter(|topic| !emitted_topics.contains(*topic))
        .collect();
    missing_topics.sort();
    for topic in missing_topics {
        let Some(group) = effective_records.get(topic) else {
            continue;
        };
        let Some(live_ids) = live_infos.get(topic) else {
            continue;
        };
        if group.dialogue.id.is_empty() {
            continue;
        }
        audit.add_materialized_topic(topic, live_ids.len(), live_topics.get(topic));
        rebuilt.push(TES3Object::Dialogue(group.dialogue.clone()));
        append_live_dialogue_infos(&mut rebuilt, group, live_ids);
    }
    plugin.objects = rebuilt;
    audit.print();
}

fn append_live_dialogue_infos(
    rebuilt: &mut Vec<TES3Object>,
    group: &DialogueGroup,
    live_ids: &HashSet<String>,
) {
    let survivors: Vec<_> = group
        .infos
        .iter()
        .filter(|info| !info.deleted() && live_ids.contains(&info.id.to_ascii_lowercase()))
        .collect();
    for (index, info) in survivors.iter().enumerate() {
        let mut info = (*info).clone();
        info.prev_id = index
            .checked_sub(1)
            .and_then(|index| survivors.get(index))
            .map_or_else(String::new, |info| info.id.clone());
        info.next_id = survivors
            .get(index + 1)
            .map_or_else(String::new, |info| info.id.clone());
        rebuilt.push(TES3Object::DialogueInfo(info));
    }
}

struct DialoguePopulation {
    actors: Vec<DialogueActor>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum DialogueTopicReason {
    Starwind,
    Engine,
    Script,
}

impl DialogueTopicReason {
    fn name(self) -> &'static str {
        match self {
            Self::Starwind => "StarwindTopic",
            Self::Engine => "EngineTopic",
            Self::Script => "ScriptTopic",
        }
    }
}

type DialogueTopicReasons = HashMap<String, HashSet<DialogueTopicReason>>;

#[derive(Default)]
struct DialogueAudit {
    effective_infos: usize,
    starwind_owned_infos: usize,
    retained_vanilla_infos: usize,
    pruned_vanilla_infos: usize,
    keep_reasons: BTreeMap<&'static str, usize>,
    materialized_vanilla_topics: BTreeMap<String, (usize, Vec<&'static str>)>,
}

impl DialogueAudit {
    fn add_reason(&mut self, reason: &'static str) {
        *self.keep_reasons.entry(reason).or_default() += 1;
    }

    fn add_materialized_topic(
        &mut self,
        topic: &str,
        live_info_count: usize,
        reasons: Option<&HashSet<DialogueTopicReason>>,
    ) {
        let mut reasons: Vec<_> = reasons
            .into_iter()
            .flatten()
            .map(|reason| reason.name())
            .collect();
        reasons.sort_unstable();
        self.materialized_vanilla_topics
            .insert(topic.to_string(), (live_info_count, reasons));
    }

    fn print(&self) {
        println!(
            "Dialogue INFO audit: effective={}, starwind_owned={}, retained_vanilla={}, pruned_vanilla={}",
            self.effective_infos,
            self.starwind_owned_infos,
            self.retained_vanilla_infos,
            self.pruned_vanilla_infos,
        );
        for (reason, count) in &self.keep_reasons {
            println!("Dialogue INFO keep reason: {reason}={count}");
        }
        for (topic, (live_info_count, reasons)) in &self.materialized_vanilla_topics {
            println!(
                "Dialogue DIAL materialized: topic={topic:?} source=vanilla live_infos={live_info_count} topic_reason={reasons:?}"
            );
        }
    }
}

struct DialogueActor {
    id: String,
    race: String,
    class: String,
}

fn collect_dialogue_population(plugin: &Plugin) -> DialoguePopulation {
    let mut population = DialoguePopulation { actors: Vec::new() };
    for object in &plugin.objects {
        if object.deleted() {
            continue;
        }
        match object {
            TES3Object::Npc(npc) => population.actors.push(DialogueActor {
                id: npc.id.to_ascii_lowercase(),
                race: npc.race.to_ascii_lowercase(),
                class: npc.class.to_ascii_lowercase(),
            }),
            TES3Object::Creature(creature) => population.actors.push(DialogueActor {
                id: creature.id.to_ascii_lowercase(),
                race: String::new(),
                class: String::new(),
            }),
            _ => {}
        }
    }
    population
}

fn print_dialogue_liveness_population(plugin: &Plugin) {
    println!("Dialogue liveness population begin");
    for object in &plugin.objects {
        if object.deleted() {
            continue;
        }
        match object {
            TES3Object::Npc(npc) => println!(
                "Dialogue liveness actor\tNpc\t{}\t{}\t{}\t{}",
                npc.id,
                npc.race,
                npc.class,
                if npc.npc_flags.contains(NpcFlags::FEMALE) {
                    "female"
                } else {
                    "male"
                }
            ),
            TES3Object::Creature(creature) => {
                println!("Dialogue liveness actor\tCreature\t{}\t\t\t", creature.id);
            }
            _ => {}
        }
    }
    println!("Dialogue liveness population end");
}

fn collect_live_dialogue_topics(
    plugin: &Plugin,
    records: &DialogueRecords,
    starwind_ids: &HashMap<String, HashSet<String>>,
) -> DialogueTopicReasons {
    let mut live_topics = DialogueTopicReasons::new();
    for topic in starwind_ids.keys() {
        live_topics
            .entry(topic.clone())
            .or_default()
            .insert(DialogueTopicReason::Starwind);
    }
    for (topic, group) in records {
        if matches!(
            group.dialogue_type,
            DialogueType2::Greeting | DialogueType2::Voice | DialogueType2::Persuasion
        ) {
            live_topics
                .entry(topic.clone())
                .or_default()
                .insert(DialogueTopicReason::Engine);
        }
    }

    let topic_ids: Vec<_> = records
        .keys()
        .filter(|topic| !topic.is_empty())
        .cloned()
        .collect();
    let script_texts: Vec<_> = plugin
        .objects
        .iter()
        .filter_map(|object| match object {
            TES3Object::Script(script) => Some(script.text.as_str()),
            _ => None,
        })
        .collect();
    for text in &script_texts {
        add_script_topic_references(text, &topic_ids, &mut live_topics);
    }
    live_topics
}

fn add_script_topic_references(
    text: &str,
    topic_ids: &[String],
    live_topics: &mut DialogueTopicReasons,
) {
    for line in text.lines() {
        let line = line.trim_start();
        if line.starts_with(';') || line.len() < "AddTopic".len() {
            continue;
        }
        let (command, rest) = line.split_at("AddTopic".len());
        if !command.eq_ignore_ascii_case("AddTopic")
            || !rest.chars().next().is_some_and(char::is_whitespace)
        {
            continue;
        }
        let operand = rest.trim_start();
        let operand = if let Some(operand) = operand.strip_prefix('"') {
            operand.split('"').next().unwrap_or_default()
        } else {
            operand.split_whitespace().next().unwrap_or_default()
        };
        if let Some(topic) = topic_ids
            .iter()
            .find(|topic| topic.eq_ignore_ascii_case(operand))
        {
            live_topics
                .entry(topic.clone())
                .or_default()
                .insert(DialogueTopicReason::Script);
        }
    }
}

fn collect_live_dialogue_infos(
    records: &DialogueRecords,
    starwind_ids: &HashMap<String, HashSet<String>>,
    live_topics: &DialogueTopicReasons,
    population: &DialoguePopulation,
) -> (HashMap<String, HashSet<String>>, DialogueAudit) {
    let mut live_infos = HashMap::new();
    let mut audit = DialogueAudit::default();
    for (topic, group) in records {
        let starwind_topic_ids = starwind_ids.get(topic);
        let topic_reasons = live_topics.get(topic);
        for info in &group.infos {
            let id = info.id.to_ascii_lowercase();
            if info.deleted() {
                continue;
            }
            audit.effective_infos += 1;
            let starwind_owned = starwind_topic_ids.is_some_and(|ids| ids.contains(&id));
            if starwind_owned {
                audit.starwind_owned_infos += 1;
            }
            let live = topic_reasons.is_some()
                && (starwind_owned || info_can_match_actor(info, population));
            if live {
                live_infos
                    .entry(topic.clone())
                    .or_insert_with(HashSet::new)
                    .insert(id);
                if !starwind_owned {
                    audit.retained_vanilla_infos += 1;
                    add_dialogue_keep_reasons(&mut audit, topic_reasons, group, info);
                }
            } else if !starwind_owned {
                audit.pruned_vanilla_infos += 1;
            }
        }
    }
    (live_infos, audit)
}

fn add_dialogue_keep_reasons(
    audit: &mut DialogueAudit,
    topic_reasons: Option<&HashSet<DialogueTopicReason>>,
    group: &DialogueGroup,
    info: &DialogueInfo,
) {
    let speaker_id = info.speaker_id.to_ascii_lowercase();
    let race = info.speaker_race.to_ascii_lowercase();
    let class = info.speaker_class.to_ascii_lowercase();
    let faction = info.speaker_faction.to_ascii_lowercase();
    if speaker_id.is_empty()
        && race.is_empty()
        && class.is_empty()
        && faction.is_empty()
        && matches!(
            group.dialogue_type,
            DialogueType2::Greeting | DialogueType2::Voice | DialogueType2::Persuasion
        )
    {
        audit.add_reason("VANILLA_ENGINE_TOPIC_GENERIC");
    }
    if !speaker_id.is_empty() {
        audit.add_reason("VANILLA_MATCHING_SPEAKER_ID");
    }
    if !race.is_empty() {
        audit.add_reason("VANILLA_MATCHING_RACE");
    }
    if !class.is_empty() {
        audit.add_reason("VANILLA_MATCHING_CLASS");
    }
    if !faction.is_empty() {
        audit.add_reason("VANILLA_FACTION_UNKNOWN");
    }
    if topic_reasons.is_some_and(|reasons| reasons.contains(&DialogueTopicReason::Script)) {
        audit.add_reason("VANILLA_SCRIPT_TOPIC");
    }
    if topic_reasons.is_some_and(|reasons| reasons.contains(&DialogueTopicReason::Starwind)) {
        audit.add_reason("VANILLA_DIALOGUE_TOPIC");
    }
}

fn info_can_match_actor(info: &DialogueInfo, population: &DialoguePopulation) -> bool {
    let speaker_id = info.speaker_id.to_ascii_lowercase();
    let race = info.speaker_race.to_ascii_lowercase();
    let class = info.speaker_class.to_ascii_lowercase();
    let faction = info.speaker_faction.to_ascii_lowercase();
    let has_known_constraints = !speaker_id.is_empty() || !race.is_empty() || !class.is_empty();
    let has_candidate = population.actors.iter().any(|actor| {
        (speaker_id.is_empty() || same_id(&actor.id, &speaker_id))
            && (race.is_empty() || same_id(&actor.race, &race))
            && (class.is_empty() || same_id(&actor.class, &class))
    });
    if has_known_constraints && !has_candidate {
        return false;
    }
    if !faction.is_empty() {
        // Runtime faction membership and rank are not represented by an actor
        // definition, so faction constraints cannot prove an INFO impossible.
        return true;
    }
    true
}

fn collect_effective_dialogue_records(plugin: &Plugin, base_plugins: &[Plugin]) -> DialogueRecords {
    let mut records = HashMap::new();
    for base_plugin in base_plugins.iter().rev() {
        merge_dialogue_records(base_plugin, &mut records);
    }
    merge_dialogue_records(plugin, &mut records);
    records
}

fn merge_dialogue_records(plugin: &Plugin, records: &mut DialogueRecords) {
    let mut topic = None;
    for object in &plugin.objects {
        match object {
            TES3Object::Dialogue(dialogue) => {
                let group = records.entry(dialogue.id.to_ascii_lowercase()).or_default();
                group.dialogue = dialogue.clone();
                group.dialogue_type = dialogue.dialogue_type;
                topic = Some(dialogue.id.to_ascii_lowercase());
            }
            TES3Object::DialogueInfo(info) => {
                let Some(topic) = topic.as_ref() else {
                    continue;
                };
                let id = info.id.to_ascii_lowercase();
                if id.is_empty() {
                    continue;
                }
                records
                    .entry(topic.clone())
                    .or_default()
                    .insert_info(info.clone());
            }
            _ => topic = None,
        }
    }
}

fn collect_dialogue_ids(plugin: &Plugin) -> HashMap<String, HashSet<String>> {
    let mut ids: HashMap<String, HashSet<String>> = HashMap::new();
    let mut topic = None;
    for object in &plugin.objects {
        match object {
            TES3Object::Dialogue(dialogue) => {
                let id = dialogue.id.to_ascii_lowercase();
                ids.entry(id.clone()).or_default();
                topic = Some(id);
            }
            TES3Object::DialogueInfo(info) => {
                let Some(topic) = topic.as_ref() else {
                    continue;
                };
                let id = info.id.to_ascii_lowercase();
                if !id.is_empty() && !info.deleted() {
                    ids.entry(topic.clone()).or_default().insert(id);
                }
            }
            _ => topic = None,
        }
    }
    ids
}

struct VanillaIndex<'a> {
    masters: Vec<HashMap<String, &'a TES3Object>>,
}

impl<'a> VanillaIndex<'a> {
    fn new(base_plugins: &'a [Plugin]) -> Self {
        let masters = base_plugins
            .iter()
            .map(|plugin| {
                let mut records = HashMap::new();
                for object in &plugin.objects {
                    if !never_copy(object) && !object.deleted() {
                        records
                            .entry(object.editor_id().to_ascii_lowercase())
                            .or_insert(object);
                    }
                }
                records
            })
            .collect();
        Self { masters }
    }

    fn resolve(&self, id: &str) -> Option<(usize, &'a TES3Object)> {
        self.masters
            .iter()
            .enumerate()
            .find_map(|(index, records)| records.get(id).map(|object| (index, *object)))
    }
}

fn add_vanilla_refs(
    plugin: &mut Plugin,
    base_plugins: &[Plugin],
    defined_ids: &mut HashSet<String>,
    defined_effects: &mut HashSet<EffectId>,
    pass: &'static str,
) {
    add_vanilla_refs_with_dialogue(
        plugin,
        base_plugins,
        defined_ids,
        defined_effects,
        true,
        pass,
    );
}

fn add_vanilla_refs_without_dialogue(
    plugin: &mut Plugin,
    base_plugins: &[Plugin],
    defined_ids: &mut HashSet<String>,
    defined_effects: &mut HashSet<EffectId>,
    pass: &'static str,
) {
    add_vanilla_refs_with_dialogue(
        plugin,
        base_plugins,
        defined_ids,
        defined_effects,
        false,
        pass,
    );
}

fn add_vanilla_refs_with_dialogue(
    plugin: &mut Plugin,
    base_plugins: &[Plugin],
    defined_ids: &mut HashSet<String>,
    defined_effects: &mut HashSet<EffectId>,
    include_dialogue_dependencies: bool,
    pass: &'static str,
) {
    let vanilla_index = VanillaIndex::new(base_plugins);
    let mut required_ids = HashSet::new();
    let mut pending_ids = VecDeque::new();
    for object in plugin.objects.iter().filter(|object| {
        !object.deleted()
            && (include_dialogue_dependencies || !matches!(object, TES3Object::DialogueInfo(_)))
    }) {
        enqueue_dependencies(object, &mut required_ids, &mut pending_ids, pass);
    }
    pending_ids
        .make_contiguous()
        .sort_by(|left, right| left.id.cmp(&right.id));

    // Resolve the graph to a fixpoint. Every copied record can introduce more
    // references, so those references must go back through the same resolver.
    resolve_required_ids(
        plugin,
        &vanilla_index,
        defined_ids,
        &mut required_ids,
        &mut pending_ids,
        pass,
    );

    // These record classes are intentionally imported wholesale. Their own
    // references still need to participate in the transitive resolution.
    import_vanilla_foundations(
        plugin,
        base_plugins,
        defined_ids,
        defined_effects,
        &mut required_ids,
        &mut pending_ids,
        pass,
    );
    resolve_required_ids(
        plugin,
        &vanilla_index,
        defined_ids,
        &mut required_ids,
        &mut pending_ids,
        pass,
    );
}

fn resolve_required_ids(
    plugin: &mut Plugin,
    vanilla_index: &VanillaIndex<'_>,
    defined_ids: &mut HashSet<String>,
    required_ids: &mut HashSet<String>,
    pending_ids: &mut VecDeque<PendingDependency>,
    pass: &'static str,
) {
    while let Some(pending) = pending_ids.pop_front() {
        let id = pending.id;
        if defined_ids.contains(&id) {
            continue;
        }

        let source = vanilla_index.resolve(&id);

        let Some((source_index, object)) = source else {
            continue;
        };

        let object_id = object.editor_id().to_ascii_lowercase();
        if !defined_ids.insert(object_id) {
            continue;
        }

        println!(
            "Copying '{}' ({}) to 'Starwind.esp' from '{}'",
            object.editor_id(),
            object.tag_str(),
            VANILLA_PLUGIN_NAMES
                .get(source_index)
                .copied()
                .unwrap_or("master")
        );
        let cause = pending.cause;
        log_dependency_import(object, &cause);
        enqueue_dependencies(object, required_ids, pending_ids, pass);
        plugin.objects.push(object.clone());
    }
}

fn import_vanilla_foundations(
    plugin: &mut Plugin,
    base_plugins: &[Plugin],
    defined_ids: &mut HashSet<String>,
    defined_effects: &mut HashSet<EffectId>,
    required_ids: &mut HashSet<String>,
    pending_ids: &mut VecDeque<PendingDependency>,
    pass: &'static str,
) {
    for (source_index, base_plugin) in base_plugins.iter().enumerate() {
        for object in &base_plugin.objects {
            if object.deleted() {
                continue;
            }
            if !matches!(
                object,
                TES3Object::GameSetting(_)
                    | TES3Object::MagicEffect(_)
                    | TES3Object::Race(_)
                    | TES3Object::Class(_)
            ) {
                continue;
            }

            if let TES3Object::MagicEffect(effect) = object {
                if defined_effects.insert(effect.effect_id) {
                    log_dependency_import(
                        object,
                        &DependencyCause {
                            pass,
                            source_type: "FOUNDATION".to_string(),
                            source_id: VANILLA_PLUGIN_NAMES
                                .get(source_index)
                                .copied()
                                .unwrap_or("master")
                                .to_string(),
                            field: "foundation",
                        },
                    );
                    enqueue_dependencies(object, required_ids, pending_ids, pass);
                    plugin.objects.push(object.clone());
                }
            } else {
                let id = object.editor_id().to_ascii_lowercase();
                if defined_ids.insert(id) {
                    log_dependency_import(
                        object,
                        &DependencyCause {
                            pass,
                            source_type: "FOUNDATION".to_string(),
                            source_id: VANILLA_PLUGIN_NAMES
                                .get(source_index)
                                .copied()
                                .unwrap_or("master")
                                .to_string(),
                            field: "foundation",
                        },
                    );
                    enqueue_dependencies(object, required_ids, pending_ids, pass);
                    plugin.objects.push(object.clone());
                }
            }
        }
    }
}

fn log_dependency_import(object: &TES3Object, cause: &DependencyCause) {
    println!(
        "Dependency import:\n  pass={}\n  target={} '{}'\n  source={} '{}'\n  field={}",
        cause.pass,
        object.tag_str(),
        object.editor_id(),
        cause.source_type,
        cause.source_id,
        cause.field,
    );
}

#[derive(Clone)]
struct DependencyCause {
    pass: &'static str,
    source_type: String,
    source_id: String,
    field: &'static str,
}

struct PendingDependency {
    id: String,
    cause: DependencyCause,
}

fn dependency_field(object: &TES3Object, target: &str) -> &'static str {
    let matches = |value: &str| value.eq_ignore_ascii_case(target);
    match object {
        TES3Object::DialogueInfo(record) => dependency_field_dialogue_info(record, &matches),
        TES3Object::Cell(record) => dependency_field_cell(record, &matches),
        TES3Object::Npc(record) => dependency_field_npc(record, &matches),
        TES3Object::Creature(record) => dependency_field_creature(record, &matches),
        _ => "dependency",
    }
}

fn dependency_field_dialogue_info(
    record: &DialogueInfo,
    matches: &impl Fn(&str) -> bool,
) -> &'static str {
    if matches(&record.speaker_id) {
        "speaker_id"
    } else if matches(&record.speaker_race) {
        "speaker_race"
    } else if matches(&record.speaker_class) {
        "speaker_class"
    } else if matches(&record.speaker_faction) {
        "speaker_faction"
    } else {
        "player_faction"
    }
}

fn dependency_field_cell(
    record: &tes3::esp::Cell,
    matches: &impl Fn(&str) -> bool,
) -> &'static str {
    for reference in record.references.values() {
        if matches(&reference.id) {
            return "reference.id";
        }
        if reference.owner.as_deref().is_some_and(matches) {
            return "reference.owner";
        }
        if reference.owner_global.as_deref().is_some_and(matches) {
            return "reference.owner_global";
        }
        if reference.owner_faction.as_deref().is_some_and(matches) {
            return "reference.owner_faction";
        }
        if reference.key.as_deref().is_some_and(matches) {
            return "reference.key";
        }
        if reference.trap.as_deref().is_some_and(matches) {
            return "reference.trap";
        }
        if reference.soul.as_deref().is_some_and(matches) {
            return "reference.soul";
        }
    }
    "region"
}

fn dependency_field_npc(record: &tes3::esp::Npc, matches: &impl Fn(&str) -> bool) -> &'static str {
    if matches(&record.script) {
        "script"
    } else if record.inventory.iter().any(|(_, value)| matches(value)) {
        "inventory"
    } else if record.spells.iter().any(|value| matches(value)) {
        "spells"
    } else if record.ai_packages.iter().any(|package| match package {
        AiPackage::Activate(value) => matches(&value.target),
        AiPackage::Escort(value) => matches(&value.target),
        AiPackage::Follow(value) => matches(&value.target),
        _ => false,
    }) {
        "ai_package.target"
    } else if matches(&record.race) {
        "race"
    } else if matches(&record.class) {
        "class"
    } else if matches(&record.faction) {
        "faction"
    } else if matches(&record.head) {
        "head"
    } else if matches(&record.hair) {
        "hair"
    } else {
        "dependency"
    }
}

fn dependency_field_creature(
    record: &tes3::esp::Creature,
    matches: &impl Fn(&str) -> bool,
) -> &'static str {
    if matches(&record.script) {
        "script"
    } else if record.inventory.iter().any(|(_, value)| matches(value)) {
        "inventory"
    } else if record.spells.iter().any(|value| matches(value)) {
        "spells"
    } else if record.ai_packages.iter().any(|package| match package {
        AiPackage::Activate(value) => matches(&value.target),
        AiPackage::Escort(value) => matches(&value.target),
        AiPackage::Follow(value) => matches(&value.target),
        _ => false,
    }) {
        "ai_package.target"
    } else {
        "sound"
    }
}

fn source_id(object: &TES3Object) -> String {
    let editor_id = object.editor_id();
    if !editor_id.is_empty() {
        return editor_id.to_string();
    }
    if let TES3Object::Cell(cell) = object {
        return cell.name.clone();
    }
    "<anonymous>".to_string()
}

fn enqueue_dependencies(
    object: &TES3Object,
    required_ids: &mut HashSet<String>,
    pending_ids: &mut VecDeque<PendingDependency>,
    pass: &'static str,
) {
    let mut dependencies: Vec<_> = collect_required_ids_from_object(object)
        .into_iter()
        .collect();
    dependencies.sort();
    for id in dependencies {
        if !id.is_empty() && required_ids.insert(id.clone()) {
            pending_ids.push_back(PendingDependency {
                cause: DependencyCause {
                    pass,
                    source_type: object.tag_str().to_string(),
                    source_id: source_id(object),
                    field: dependency_field(object, &id),
                },
                id,
            });
        }
    }
}

fn collect_defined_ids(plugin: &Plugin) -> HashSet<String> {
    let mut results = HashSet::new();
    for object in &plugin.objects {
        if !never_copy(object) {
            results.insert(object.editor_id().to_ascii_lowercase());
        }
    }
    results
}

#[cfg(test)]
fn collect_required_ids(plugin: &Plugin) -> HashSet<String> {
    collect_required_ids_with_dialogue(plugin, true)
}

#[cfg(test)]
fn collect_required_ids_with_dialogue(
    plugin: &Plugin,
    include_dialogue_dependencies: bool,
) -> HashSet<String> {
    plugin
        .objects
        .iter()
        .filter(|object| !object.deleted())
        .filter(|object| {
            include_dialogue_dependencies || !matches!(object, TES3Object::DialogueInfo(_))
        })
        .flat_map(collect_required_ids_from_object)
        .collect()
}

fn strip_deleted_records(plugin: &mut Plugin) {
    plugin.objects.retain(|object| !object.deleted());
}

fn remove_exact_duplicate_magic_effects(plugin: &mut Plugin) {
    let mut retained = Vec::new();
    for object in plugin.objects.drain(..) {
        if let TES3Object::MagicEffect(effect) = &object {
            if retained.iter().any(
                |retained| matches!(retained, TES3Object::MagicEffect(other) if other == effect),
            ) {
                continue;
            }
        }
        retained.push(object);
    }
    plugin.objects = retained;
}

#[expect(
    clippy::too_many_lines,
    reason = "TES3 dependency fields are kept together by record type"
)]
fn collect_required_ids_from_object(object: &TES3Object) -> HashSet<String> {
    let mut results = HashSet::new();
    // Save the ids of any objects required by the current object.
    match object {
        TES3Object::Race(race) => {
            for spell in &race.spells {
                results.insert(spell.to_ascii_lowercase());
            }
        }
        TES3Object::SoundGen(soundgen) => {
            results.insert(soundgen.creature.to_ascii_lowercase());
            results.insert(soundgen.sound.to_ascii_lowercase());
        }
        TES3Object::MagicEffect(magic_effect) => {
            results.insert(magic_effect.bolt_sound.to_ascii_lowercase());
            results.insert(magic_effect.cast_sound.to_ascii_lowercase());
            results.insert(magic_effect.hit_sound.to_ascii_lowercase());
            results.insert(magic_effect.area_sound.to_ascii_lowercase());
            results.insert(magic_effect.cast_visual.to_ascii_lowercase());
            results.insert(magic_effect.bolt_visual.to_ascii_lowercase());
            results.insert(magic_effect.hit_visual.to_ascii_lowercase());
            results.insert(magic_effect.area_visual.to_ascii_lowercase());
        }
        TES3Object::Region(region) => {
            results.insert(region.id.clone().to_ascii_lowercase());
            results.insert(region.sleep_creature.to_ascii_lowercase());
            for (sound, _) in &region.sounds {
                results.insert(sound.to_ascii_lowercase());
            }
        }
        TES3Object::Birthsign(birthsign) => {
            for spell in &birthsign.spells {
                results.insert(spell.to_ascii_lowercase());
            }
        }
        TES3Object::Door(door) => {
            results.insert(door.script.to_ascii_lowercase());
            results.insert(door.open_sound.to_ascii_lowercase());
            results.insert(door.close_sound.to_ascii_lowercase());
        }
        TES3Object::MiscItem(misc_item) => {
            results.insert(misc_item.script.to_ascii_lowercase());
        }
        TES3Object::Weapon(weapon) => {
            results.insert(weapon.script.to_ascii_lowercase());
            results.insert(weapon.enchanting.to_ascii_lowercase());
        }
        TES3Object::Container(container) => {
            results.insert(container.script.to_ascii_lowercase());
            for item in &container.inventory {
                results.insert(item.1.to_ascii_lowercase());
            }
        }
        TES3Object::Creature(creature) => {
            results.insert(creature.script.to_ascii_lowercase());
            for (_, item) in &creature.inventory {
                results.insert(item.to_ascii_lowercase());
            }
            for spell in &creature.spells {
                results.insert(spell.to_ascii_lowercase());
            }
            // Escort/Follow targets are actor IDs; this extends the old collector.
            for package in &creature.ai_packages {
                match package {
                    AiPackage::Activate(activate) => {
                        results.insert(activate.target.to_ascii_lowercase());
                    }
                    AiPackage::Escort(escort) => {
                        results.insert(escort.target.to_ascii_lowercase());
                    }
                    AiPackage::Follow(follow) => {
                        results.insert(follow.target.to_ascii_lowercase());
                    }
                    _ => {}
                }
            }
            results.insert(creature.sound.to_ascii_lowercase());
        }
        TES3Object::Bodypart(bodypart) => {
            results.insert(bodypart.race.to_ascii_lowercase()); // should be named `.race`
        }
        TES3Object::Light(light) => {
            results.insert(light.script.to_ascii_lowercase());
            results.insert(light.sound.to_ascii_lowercase());
        }
        TES3Object::Npc(npc) => {
            results.insert(npc.script.to_ascii_lowercase());
            for (_, item) in &npc.inventory {
                results.insert(item.to_ascii_lowercase());
            }
            for spell in &npc.spells {
                results.insert(spell.to_ascii_lowercase());
            }
            // Escort/Follow targets are actor IDs; their cell fields are sanitized below.
            for package in &npc.ai_packages {
                match package {
                    AiPackage::Activate(activate) => {
                        results.insert(activate.target.to_ascii_lowercase());
                        println!(
                            "{} added as an activation target",
                            activate.target.to_ascii_lowercase()
                        );
                    }
                    AiPackage::Escort(escort) => {
                        results.insert(escort.target.to_ascii_lowercase());
                    }
                    AiPackage::Follow(follow) => {
                        results.insert(follow.target.to_ascii_lowercase());
                    }
                    _ => {}
                }
            }
            results.insert(npc.race.to_ascii_lowercase());
            results.insert(npc.class.to_ascii_lowercase());
            results.insert(npc.faction.to_ascii_lowercase());
            results.insert(npc.head.to_ascii_lowercase());
            results.insert(npc.hair.to_ascii_lowercase());
        }
        TES3Object::Armor(armor) => {
            results.insert(armor.script.to_ascii_lowercase());
            results.insert(armor.enchanting.to_ascii_lowercase());
            for biped_object in &armor.biped_objects {
                results.insert(biped_object.male_bodypart.to_ascii_lowercase());
                results.insert(biped_object.female_bodypart.to_ascii_lowercase());
            }
        }
        TES3Object::Clothing(clothing) => {
            results.insert(clothing.script.to_ascii_lowercase());
            results.insert(clothing.enchanting.to_ascii_lowercase());
            for biped_object in &clothing.biped_objects {
                results.insert(biped_object.male_bodypart.to_ascii_lowercase());
                results.insert(biped_object.female_bodypart.to_ascii_lowercase());
            }
        }
        TES3Object::RepairItem(repair_item) => {
            results.insert(repair_item.script.to_ascii_lowercase());
        }
        TES3Object::Activator(activator) => {
            results.insert(activator.script.to_ascii_lowercase());
        }
        TES3Object::Apparatus(apparatus) => {
            results.insert(apparatus.script.to_ascii_lowercase());
        }
        TES3Object::Lockpick(lockpick) => {
            results.insert(lockpick.script.to_ascii_lowercase());
        }
        TES3Object::Probe(probe) => {
            results.insert(probe.script.to_ascii_lowercase());
        }
        TES3Object::Ingredient(ingredient) => {
            results.insert(ingredient.script.to_ascii_lowercase());
        }
        TES3Object::Book(book) => {
            results.insert(book.script.to_ascii_lowercase());
            results.insert(book.enchanting.to_ascii_lowercase());
        }
        TES3Object::Alchemy(alchemy) => {
            results.insert(alchemy.script.to_ascii_lowercase());
        }
        TES3Object::LeveledItem(leveled_item) => {
            for (item, _) in &leveled_item.items {
                results.insert(item.to_ascii_lowercase());
            }
        }
        TES3Object::LeveledCreature(leveled_creature) => {
            for (creature, _) in &leveled_creature.creatures {
                results.insert(creature.to_ascii_lowercase());
            }
        }
        TES3Object::Cell(cell) => {
            if let Some(region) = &cell.region {
                results.insert(region.to_ascii_lowercase());
            }
            for reference in cell.references.values() {
                results.insert(reference.id.to_ascii_lowercase());
                println!(
                    "{} added to Starwind.esp as a cell reference in {}",
                    reference.id, cell.name
                );
                if let Some(owner) = &reference.owner {
                    results.insert(owner.to_ascii_lowercase());
                    println!(
                        "{} added to Starwind.esp as an owner reference",
                        owner.to_ascii_lowercase()
                    );
                }
                if let Some(owner_global) = &reference.owner_global {
                    results.insert(owner_global.to_ascii_lowercase());
                    println!(
                        "{} added to Starwind.esp as an owner reference",
                        owner_global.to_ascii_lowercase()
                    );
                }
                if let Some(owner_faction) = &reference.owner_faction {
                    results.insert(owner_faction.to_ascii_lowercase());
                    println!(
                        "{} added to Starwind.esp as an owner reference",
                        owner_faction.to_ascii_lowercase()
                    );
                }
                if let Some(key) = &reference.key {
                    results.insert(key.to_ascii_lowercase());
                }
                if let Some(trap) = &reference.trap {
                    results.insert(trap.to_ascii_lowercase());
                }
                if let Some(soul) = &reference.soul {
                    results.insert(soul.to_ascii_lowercase());
                }
            }
        }
        TES3Object::DialogueInfo(dialogue_info) => {
            results.insert(dialogue_info.speaker_id.to_ascii_lowercase());
            println!(
                "{} imported as a line spoken by {}",
                dialogue_info.id,
                dialogue_info.speaker_id.to_ascii_lowercase()
            );
            results.insert(dialogue_info.speaker_race.to_ascii_lowercase());
            results.insert(dialogue_info.speaker_class.to_ascii_lowercase());
            results.insert(dialogue_info.speaker_faction.to_ascii_lowercase());
            results.insert(dialogue_info.player_faction.to_ascii_lowercase());
        }
        // TES3Object::Header(_) => {},
        // TES3Object::GameSetting(_) => {},
        // TES3Object::GlobalVariable(_) => {},
        // TES3Object::Class(_) => {},
        TES3Object::Faction(faction) => {
            results.insert(faction.id.to_ascii_lowercase());
            for reaction in &faction.reactions {
                results.insert(reaction.faction.clone().to_ascii_lowercase());
            }
        }
        TES3Object::Sound(sound) => {
            results.insert(sound.id.to_ascii_lowercase());
        }
        // TES3Object::Skill(_) => {},
        // TES3Object::Script(_) => {},
        // TES3Object::StartScript(_) => {},
        // TES3Object::LandscapeTexture(_) => {},
        // TES3Object::Spell(_) => {},
        // TES3Object::Static(_) => {},
        // TES3Object::Enchanting(_) => {},
        // TES3Object::Landscape(_) => {},
        // TES3Object::PathGrid(_) => {},
        // TES3Object::Dialogue(_) => {},
        _ => {}
    }
    results.retain(|id| !id.is_empty());
    results
}

fn never_copy(object: &TES3Object) -> bool {
    matches!(
        object,
        TES3Object::Header(_)
            | TES3Object::Skill(_)
            | TES3Object::StartScript(_)
            | TES3Object::LandscapeTexture(_)
            | TES3Object::Landscape(_)
            | TES3Object::PathGrid(_)
            | TES3Object::Dialogue(_)
            | TES3Object::DialogueInfo(_)
            | TES3Object::Cell(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tes3::esp::{
        AiEscortPackage, AiFollowPackage, Dialogue, Faction, Filter, MiscItem, Npc, ObjectFlags,
        Race, Script, Spell, Static,
    };

    fn misc_item(id: &str, script: &str) -> TES3Object {
        TES3Object::MiscItem(MiscItem {
            id: id.to_string(),
            script: script.to_string(),
            ..Default::default()
        })
    }

    fn dialogue(id: &str) -> TES3Object {
        TES3Object::Dialogue(Dialogue {
            id: id.to_string(),
            ..Default::default()
        })
    }

    fn dialogue_with_type(id: &str, dialogue_type: DialogueType2) -> TES3Object {
        TES3Object::Dialogue(Dialogue {
            id: id.to_string(),
            dialogue_type,
            ..Default::default()
        })
    }

    fn dialogue_info(id: &str, previous: &str, next: &str) -> TES3Object {
        TES3Object::DialogueInfo(dialogue_info_value(id, previous, next))
    }

    fn dialogue_info_with_speaker(
        id: &str,
        previous: &str,
        next: &str,
        speaker_id: &str,
    ) -> TES3Object {
        let mut info = dialogue_info_value(id, previous, next);
        info.speaker_id = speaker_id.to_string();
        TES3Object::DialogueInfo(info)
    }

    fn dialogue_info_value(id: &str, previous: &str, next: &str) -> DialogueInfo {
        DialogueInfo {
            id: id.to_string(),
            prev_id: previous.to_string(),
            next_id: next.to_string(),
            ..Default::default()
        }
    }

    fn dialogue_info_with_constraints(
        id: &str,
        speaker_id: &str,
        race: &str,
        class: &str,
        faction: &str,
    ) -> TES3Object {
        TES3Object::DialogueInfo(DialogueInfo {
            id: id.to_string(),
            speaker_id: speaker_id.to_string(),
            speaker_race: race.to_string(),
            speaker_class: class.to_string(),
            speaker_faction: faction.to_string(),
            ..Default::default()
        })
    }

    fn has_id(plugin: &Plugin, id: &str) -> bool {
        plugin
            .objects
            .iter()
            .any(|object| object.editor_id().eq_ignore_ascii_case(id))
    }

    #[test]
    fn imported_records_are_resolved_transitively() {
        let mut plugin = Plugin::new();
        plugin.objects.push(misc_item("root", "first_dependency"));

        let bloodmoon = Plugin {
            objects: vec![misc_item("first_dependency", "second_dependency")],
        };
        let tribunal = Plugin {
            objects: vec![misc_item("second_dependency", "third_dependency")],
        };
        let morrowind = Plugin {
            objects: vec![misc_item("third_dependency", "")],
        };
        let masters = vec![bloodmoon, tribunal, morrowind];
        let mut defined_ids = collect_defined_ids(&plugin);
        let mut defined_effects = HashSet::new();

        add_vanilla_refs(
            &mut plugin,
            &masters,
            &mut defined_ids,
            &mut defined_effects,
            "post-dialogue",
        );

        assert!(has_id(&plugin, "first_dependency"));
        assert!(has_id(&plugin, "second_dependency"));
        assert!(has_id(&plugin, "third_dependency"));
    }

    #[test]
    fn first_closure_defers_dialogue_info_dependencies() {
        let mut plugin = Plugin {
            objects: vec![
                dialogue("Topic"),
                dialogue_info_with_speaker("Info", "", "", "vanilla_speaker"),
            ],
        };
        let masters = vec![Plugin {
            objects: vec![misc_item("vanilla_speaker", "")],
        }];
        let mut defined_ids = collect_defined_ids(&plugin);
        let mut defined_effects = HashSet::new();

        add_vanilla_refs_without_dialogue(
            &mut plugin,
            &masters,
            &mut defined_ids,
            &mut defined_effects,
            "pre-dialogue",
        );

        assert!(!has_id(&plugin, "vanilla_speaker"));

        add_vanilla_refs(
            &mut plugin,
            &masters,
            &mut defined_ids,
            &mut defined_effects,
            "post-dialogue",
        );

        assert!(has_id(&plugin, "vanilla_speaker"));
    }

    #[test]
    fn deleted_records_do_not_contribute_dependencies_or_serialize() {
        let mut deleted = misc_item("deleted_record", "unwanted_dependency");
        if let TES3Object::MiscItem(item) = &mut deleted {
            item.flags.insert(ObjectFlags::DELETED);
        }
        let mut plugin = Plugin {
            objects: vec![deleted],
        };
        let masters = vec![Plugin {
            objects: vec![misc_item("unwanted_dependency", "")],
        }];
        let mut defined_ids = collect_defined_ids(&plugin);
        let mut defined_effects = HashSet::new();

        add_vanilla_refs(
            &mut plugin,
            &masters,
            &mut defined_ids,
            &mut defined_effects,
            "post-dialogue",
        );

        assert!(!has_id(&plugin, "unwanted_dependency"));
        strip_deleted_records(&mut plugin);
        assert!(plugin.objects.is_empty());
    }

    #[test]
    fn exact_duplicate_magic_effects_are_removed() {
        let effect = TES3Object::MagicEffect(MagicEffect {
            effect_id: EffectId::WaterBreathing,
            ..Default::default()
        });
        let mut plugin = Plugin {
            objects: vec![effect.clone(), effect],
        };

        remove_exact_duplicate_magic_effects(&mut plugin);

        assert_eq!(plugin.objects_of_type::<MagicEffect>().count(), 1);
    }

    #[test]
    fn required_records_keep_bloodmoon_precedence() {
        let mut plugin = Plugin::new();
        plugin.objects.push(misc_item("root", "shared_dependency"));

        let bloodmoon = Plugin {
            objects: vec![
                misc_item("shared_dependency", "bloodmoon_leaf"),
                misc_item("bloodmoon_leaf", ""),
            ],
        };
        let tribunal = Plugin {
            objects: vec![misc_item("shared_dependency", "tribunal_leaf")],
        };
        let masters = vec![bloodmoon, tribunal];
        let mut defined_ids = collect_defined_ids(&plugin);
        let mut defined_effects = HashSet::new();

        add_vanilla_refs(
            &mut plugin,
            &masters,
            &mut defined_ids,
            &mut defined_effects,
            "post-dialogue",
        );

        assert!(has_id(&plugin, "shared_dependency"));
        assert!(has_id(&plugin, "bloodmoon_leaf"));
        assert!(!has_id(&plugin, "tribunal_leaf"));
    }

    #[test]
    fn foundation_dependencies_are_resolved() {
        let mut plugin = Plugin::new();
        plugin.objects.push(misc_item("WaterBreathing", ""));
        let bloodmoon = Plugin {
            objects: vec![
                TES3Object::Race(Race {
                    id: "vanilla_race".to_string(),
                    spells: vec!["vanilla_spell".to_string()],
                    ..Default::default()
                }),
                TES3Object::Spell(Spell {
                    id: "vanilla_spell".to_string(),
                    ..Default::default()
                }),
                TES3Object::MagicEffect(MagicEffect {
                    effect_id: EffectId::WaterBreathing,
                    cast_visual: "vanilla_cast_visual".to_string(),
                    ..Default::default()
                }),
                TES3Object::Static(Static {
                    id: "vanilla_cast_visual".to_string(),
                    ..Default::default()
                }),
            ],
        };
        let mut defined_ids = collect_defined_ids(&plugin);
        let mut defined_effects = HashSet::new();

        add_vanilla_refs(
            &mut plugin,
            &[bloodmoon],
            &mut defined_ids,
            &mut defined_effects,
            "post-dialogue",
        );

        assert!(has_id(&plugin, "vanilla_race"));
        assert!(has_id(&plugin, "vanilla_spell"));
        assert!(has_id(&plugin, "vanilla_cast_visual"));
        assert_eq!(plugin.objects_of_type::<MagicEffect>().count(), 1);
        assert!(plugin
            .objects_of_type::<MagicEffect>()
            .any(|effect| effect.effect_id == EffectId::WaterBreathing));
    }

    #[test]
    fn escort_and_follow_targets_are_resolved() {
        let mut plugin = Plugin::new();
        plugin.objects.push(TES3Object::Npc(Npc {
            id: "root".to_string(),
            ai_packages: vec![
                AiPackage::Escort(AiEscortPackage {
                    target: "escort_target".to_string().into(),
                    ..Default::default()
                }),
                AiPackage::Follow(AiFollowPackage {
                    target: "follow_target".to_string().into(),
                    ..Default::default()
                }),
            ],
            ..Default::default()
        }));
        let masters = vec![Plugin {
            objects: vec![
                misc_item("escort_target", ""),
                misc_item("follow_target", ""),
            ],
        }];
        let mut defined_ids = collect_defined_ids(&plugin);
        let mut defined_effects = HashSet::new();

        add_vanilla_refs(
            &mut plugin,
            &masters,
            &mut defined_ids,
            &mut defined_effects,
            "post-dialogue",
        );

        assert!(has_id(&plugin, "escort_target"));
        assert!(has_id(&plugin, "follow_target"));
    }

    #[test]
    fn dialogue_sound_and_filter_operands_are_not_dependencies() {
        let mut plugin = Plugin::new();
        plugin.objects.push(TES3Object::DialogueInfo(DialogueInfo {
            sound_path: "sound/dialogue.wav".to_string(),
            filters: vec![Filter {
                id: "filter_operand".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        }));

        let required_ids = collect_required_ids(&plugin);

        assert!(!required_ids.contains("sound/dialogue.wav"));
        assert!(!required_ids.contains("filter_operand"));
        assert!(!required_ids.contains(""));
    }

    #[test]
    fn dialogue_links_skip_removed_vanilla_infos() {
        let mut usable_vanilla_info = dialogue_info("V2", "V1", "V3");
        if let TES3Object::DialogueInfo(info) = &mut usable_vanilla_info {
            info.speaker_cell = "MissingCell".to_string();
        }
        let masters = vec![Plugin {
            objects: vec![
                dialogue("Topic"),
                dialogue_info_with_speaker("V1", "", "V2", "missing_actor"),
                usable_vanilla_info,
                dialogue_info_with_speaker("V3", "V2", "", "missing_actor"),
            ],
        }];
        let mut first_starwind_info = dialogue_info("S1", "", "V1");
        if let TES3Object::DialogueInfo(info) = &mut first_starwind_info {
            info.speaker_cell = "VanillaCell".to_string();
            info.filters = vec![Filter {
                id: "filter_operand".to_string(),
                ..Default::default()
            }];
        }
        let mut plugin = Plugin {
            objects: vec![
                dialogue("Topic"),
                first_starwind_info,
                dialogue_info("S2", "V2", "V3"),
                dialogue_info("S3", "V3", ""),
            ],
        };

        decouple_dialogue_infos(&mut plugin, &masters);

        let infos: Vec<_> = plugin
            .objects_of_type::<DialogueInfo>()
            .map(|info| {
                (
                    info.id.as_str(),
                    info.prev_id.as_str(),
                    info.next_id.as_str(),
                )
            })
            .collect();
        assert_eq!(
            infos,
            vec![
                ("S1", "", "V2"),
                ("V2", "S1", "S2"),
                ("S2", "V2", "S3"),
                ("S3", "S2", ""),
            ]
        );
        let first_info = plugin.objects_of_type::<DialogueInfo>().next().unwrap();
        assert_eq!(first_info.speaker_cell, "VanillaCell");
        assert_eq!(first_info.filters[0].id, "filter_operand");
    }

    #[test]
    fn live_vanilla_only_topic_is_materialized() {
        let masters = vec![Plugin {
            objects: vec![
                dialogue("VanillaTopic"),
                dialogue_info("VanillaInfo", "", ""),
                dialogue("Ship"),
                dialogue_info("ShipInfo", "", ""),
            ],
        }];
        let mut plugin = Plugin {
            objects: vec![TES3Object::Script(Script {
                id: "starwind_script".to_string(),
                text: "AddTopic \"VanillaTopic\"".to_string(),
                ..Default::default()
            })],
        };

        decouple_dialogue_infos(&mut plugin, &masters);

        assert!(has_id(&plugin, "VanillaTopic"));
        assert!(has_id(&plugin, "VanillaInfo"));
        assert!(!has_id(&plugin, "Ship"));
        assert!(!has_id(&plugin, "ShipInfo"));
    }

    #[test]
    fn dialogue_text_does_not_materialize_vanilla_topics() {
        let mut price_info = dialogue_info("PriceInfo", "", "");
        if let TES3Object::DialogueInfo(info) = &mut price_info {
            info.text = "VanillaB".to_string();
        }
        let masters = vec![Plugin {
            objects: vec![
                dialogue("Price on Your Head"),
                price_info,
                dialogue("VanillaB"),
                dialogue_info("VanillaBInfo", "", ""),
            ],
        }];
        let mut starwind_info = dialogue_info("StarwindInfo", "", "");
        if let TES3Object::DialogueInfo(info) = &mut starwind_info {
            info.text = "price on your head; VanillaB".to_string();
        }
        let mut plugin = Plugin {
            objects: vec![dialogue("StarwindTopic"), starwind_info],
        };

        decouple_dialogue_infos(&mut plugin, &masters);

        assert!(!has_id(&plugin, "Price on Your Head"));
        assert!(!has_id(&plugin, "PriceInfo"));
        assert!(!has_id(&plugin, "VanillaB"));
        assert!(!has_id(&plugin, "VanillaBInfo"));
    }

    #[test]
    fn dialogue_materialization_dependencies_are_resolved_by_second_pass() {
        let base_plugins = vec![Plugin {
            objects: vec![
                dialogue_with_type("Topic", DialogueType2::Voice),
                dialogue_info_with_constraints("VanillaInfo", "", "", "", "Morag Tong"),
                TES3Object::Faction(Faction {
                    id: "Morag Tong".to_string(),
                    ..Default::default()
                }),
            ],
        }];
        let mut plugin = Plugin::new();
        let mut defined_ids = collect_defined_ids(&plugin);
        let mut defined_effects = HashSet::new();

        add_vanilla_refs(
            &mut plugin,
            &base_plugins,
            &mut defined_ids,
            &mut defined_effects,
            "post-dialogue",
        );
        decouple_dialogue_infos(&mut plugin, &base_plugins);

        assert!(has_id(&plugin, "VanillaInfo"));
        assert!(!has_id(&plugin, "Morag Tong"));

        add_vanilla_refs(
            &mut plugin,
            &base_plugins,
            &mut defined_ids,
            &mut defined_effects,
            "post-dialogue",
        );

        assert!(has_id(&plugin, "Morag Tong"));
    }

    #[test]
    fn engine_driven_voice_topic_is_materialized() {
        let masters = vec![Plugin {
            objects: vec![
                dialogue_with_type("EngineVoice", DialogueType2::Voice),
                dialogue_info("VanillaVoiceInfo", "", ""),
            ],
        }];
        let mut plugin = Plugin::new();

        decouple_dialogue_infos(&mut plugin, &masters);

        assert!(has_id(&plugin, "EngineVoice"));
        assert!(has_id(&plugin, "VanillaVoiceInfo"));
    }

    #[test]
    fn matching_actor_constraints_retain_vanilla_info() {
        let masters = vec![Plugin {
            objects: vec![
                dialogue("Topic"),
                dialogue_info_with_constraints("VanillaInfo", "", "Nord", "Warrior", ""),
            ],
        }];
        let mut plugin = Plugin {
            objects: vec![
                TES3Object::Npc(Npc {
                    id: "speaker".to_string(),
                    race: "Nord".to_string(),
                    class: "Warrior".to_string(),
                    ..Default::default()
                }),
                dialogue("Topic"),
                dialogue_info("StarwindInfo", "", ""),
            ],
        };

        decouple_dialogue_infos(&mut plugin, &masters);

        assert!(has_id(&plugin, "VanillaInfo"));
    }

    #[test]
    fn incompatible_actor_constraints_prune_vanilla_info() {
        let masters = vec![Plugin {
            objects: vec![
                dialogue("Topic"),
                dialogue_info_with_constraints("VanillaInfo", "", "Dunmer", "Warrior", ""),
            ],
        }];
        let mut plugin = Plugin {
            objects: vec![
                TES3Object::Npc(Npc {
                    id: "speaker".to_string(),
                    race: "Nord".to_string(),
                    class: "Warrior".to_string(),
                    ..Default::default()
                }),
                dialogue("Topic"),
                dialogue_info("StarwindInfo", "", ""),
            ],
        };

        decouple_dialogue_infos(&mut plugin, &masters);

        assert!(!has_id(&plugin, "VanillaInfo"));
    }

    #[test]
    fn impossible_speaker_id_prunes_info_even_with_unknown_faction() {
        let masters = vec![Plugin {
            objects: vec![
                dialogue("Topic"),
                dialogue_info_with_constraints(
                    "VanillaInfo",
                    "missing_speaker",
                    "",
                    "",
                    "some_faction",
                ),
            ],
        }];
        let mut plugin = Plugin {
            objects: vec![dialogue("Topic"), dialogue_info("StarwindInfo", "", "")],
        };

        decouple_dialogue_infos(&mut plugin, &masters);

        assert!(!has_id(&plugin, "VanillaInfo"));
    }

    #[test]
    fn dialogue_group_matches_ids_case_insensitively() {
        let mut group = DialogueGroup::default();
        group.insert_info(dialogue_info_value("First", "", ""));
        group.insert_info(dialogue_info_value("Third", "First", ""));
        group.insert_info(dialogue_info_value("second", "first", ""));

        assert_eq!(
            group
                .infos
                .iter()
                .map(|info| info.id.as_str())
                .collect::<Vec<_>>(),
            vec!["First", "second", "Third"]
        );

        group.insert_info(dialogue_info_value("FIRST", "", ""));
        assert_eq!(group.infos.len(), 3);
        assert_eq!(group.infos[0].id, "FIRST");
    }

    #[test]
    fn dialogue_group_moves_existing_info_using_openmw_insertion_point() {
        let mut group = DialogueGroup::default();
        group.insert_info(dialogue_info_value("First", "", ""));
        group.insert_info(dialogue_info_value("Second", "First", ""));

        group.insert_info(dialogue_info_value("First", "First", ""));

        assert_eq!(
            group
                .infos
                .iter()
                .map(|info| info.id.as_str())
                .collect::<Vec<_>>(),
            vec!["First", "Second"]
        );
    }

    #[test]
    fn starwind_dialogue_shell_is_preserved_without_live_infos() {
        let masters = vec![Plugin {
            objects: vec![dialogue("DeadTopic")],
        }];
        let mut plugin = Plugin {
            objects: vec![
                dialogue("DeadTopic"),
                dialogue("LiveTopic"),
                dialogue_info("LiveInfo", "", ""),
            ],
        };

        decouple_dialogue_infos(&mut plugin, &masters);

        assert!(has_id(&plugin, "DeadTopic"));
        assert!(has_id(&plugin, "LiveTopic"));
        assert!(has_id(&plugin, "LiveInfo"));
    }

    #[test]
    fn deleted_dialogue_info_suppresses_parent_without_serializing_tombstone() {
        let mut deleted_info = dialogue_info_value("VanillaInfo", "", "");
        deleted_info.flags.insert(ObjectFlags::DELETED);
        let base_plugins = vec![Plugin {
            objects: vec![dialogue("Topic"), dialogue_info("VanillaInfo", "", "")],
        }];
        let mut plugin = Plugin {
            objects: vec![dialogue("Topic"), TES3Object::DialogueInfo(deleted_info)],
        };

        decouple_dialogue_infos(&mut plugin, &base_plugins);

        assert!(has_id(&plugin, "Topic"));
        assert_eq!(plugin.objects_of_type::<DialogueInfo>().count(), 0);
    }

    #[test]
    fn duplicate_dialogue_shell_is_serialized_once() {
        let mut plugin = Plugin {
            objects: vec![
                dialogue("Topic"),
                dialogue_info("Info", "", ""),
                dialogue("Topic"),
            ],
        };

        decouple_dialogue_infos(&mut plugin, &[]);

        assert_eq!(
            plugin
                .objects
                .iter()
                .filter(|object| matches!(object, TES3Object::Dialogue(_)))
                .count(),
            1
        );
        assert_eq!(plugin.objects_of_type::<DialogueInfo>().count(), 1);
    }
}
