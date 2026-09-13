use tes3::esp::{
    AiPackage, DialogueInfo, EditorId, EffectId, FilterType, MagicEffect, Plugin, TES3Object,
    TypeInfo,
};

use std::collections::{HashMap, HashSet, VecDeque};

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
    add_vanilla_refs(
        &mut plugin,
        &base_plugins,
        &mut defined_ids,
        &mut defined_effects,
    );
    for info in plugin.objects_of_type_mut::<DialogueInfo>() {
        let cell = info.speaker_cell.to_ascii_lowercase();
        if !defined_ids.contains(&cell) {
            info.speaker_cell = "igtestcell".to_string();
        }
        info.filters.retain(|filter| {
            if filter.filter_type == FilterType::NotCell {
                let cell = filter.id.to_ascii_lowercase();
                defined_ids.contains(&cell)
            } else {
                true
            }
        });
    }

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

    decouple_dialogue_infos(&mut plugin, &base_plugins);

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
    infos: Vec<DialogueInfo>,
}

impl DialogueGroup {
    fn insert_info(&mut self, info: DialogueInfo) {
        debug_assert!(
            self.infos
                .iter()
                .filter(|existing| same_id(&existing.id, &info.id))
                .count()
                <= 1
        );
        if let Some(index) = self
            .infos
            .iter()
            .position(|existing| same_id(&existing.id, &info.id))
        {
            if same_id(&self.infos[index].prev_id, &info.prev_id) {
                self.infos[index] = info;
                return;
            }
            self.infos.remove(index);
        }

        if info.prev_id.is_empty() {
            self.infos.insert(0, info);
        } else if let Some(index) = self
            .infos
            .iter()
            .position(|existing| same_id(&existing.id, &info.prev_id))
        {
            self.infos.insert(index + 1, info);
        } else {
            self.infos.push(info);
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
    let mut links: HashMap<String, HashMap<String, (String, String)>> = HashMap::new();

    for (topic, group) in &effective_records {
        let Some(starwind_topic_ids) = starwind_ids.get(topic) else {
            continue;
        };
        let survivors: Vec<_> = group
            .infos
            .iter()
            .filter(|info| starwind_topic_ids.contains(&info.id.to_ascii_lowercase()))
            .collect();
        let topic_links = links.entry(topic.clone()).or_default();
        for (index, info) in survivors.iter().enumerate() {
            let previous = index
                .checked_sub(1)
                .and_then(|index| survivors.get(index))
                .map_or_else(String::new, |info| info.id.clone());
            let next = survivors
                .get(index + 1)
                .map_or_else(String::new, |info| info.id.clone());
            topic_links.insert(info.id.to_ascii_lowercase(), (previous, next));
        }
    }

    let mut topic = None;
    for object in &mut plugin.objects {
        match object {
            TES3Object::Dialogue(dialogue) => topic = Some(dialogue.id.to_ascii_lowercase()),
            TES3Object::DialogueInfo(info) => {
                let Some(topic) = topic.as_ref() else {
                    continue;
                };
                let id = info.id.to_ascii_lowercase();
                let Some((previous, next)) = links.get(topic).and_then(|links| links.get(&id))
                else {
                    continue;
                };
                info.prev_id.clone_from(previous);
                info.next_id.clone_from(next);
            }
            _ => {}
        }
    }
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
            TES3Object::Dialogue(dialogue) => topic = Some(dialogue.id.to_ascii_lowercase()),
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
            _ => {}
        }
    }
}

fn collect_dialogue_ids(plugin: &Plugin) -> HashMap<String, HashSet<String>> {
    let mut ids: HashMap<String, HashSet<String>> = HashMap::new();
    let mut topic = None;
    for object in &plugin.objects {
        match object {
            TES3Object::Dialogue(dialogue) => topic = Some(dialogue.id.to_ascii_lowercase()),
            TES3Object::DialogueInfo(info) => {
                let Some(topic) = topic.as_ref() else {
                    continue;
                };
                let id = info.id.to_ascii_lowercase();
                if !id.is_empty() {
                    ids.entry(topic.clone()).or_default().insert(id);
                }
            }
            _ => {}
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
                    if !never_copy(object) {
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
) {
    let vanilla_index = VanillaIndex::new(base_plugins);
    let mut required_ids = collect_required_ids(plugin);
    let mut pending_ids: VecDeque<_> = required_ids.iter().cloned().collect();
    pending_ids.make_contiguous().sort();

    // Resolve the graph to a fixpoint. Every copied record can introduce more
    // references, so those references must go back through the same resolver.
    resolve_required_ids(
        plugin,
        &vanilla_index,
        defined_ids,
        &mut required_ids,
        &mut pending_ids,
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
    );
    resolve_required_ids(
        plugin,
        &vanilla_index,
        defined_ids,
        &mut required_ids,
        &mut pending_ids,
    );
}

fn resolve_required_ids(
    plugin: &mut Plugin,
    vanilla_index: &VanillaIndex<'_>,
    defined_ids: &mut HashSet<String>,
    required_ids: &mut HashSet<String>,
    pending_ids: &mut VecDeque<String>,
) {
    while let Some(id) = pending_ids.pop_front() {
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
        enqueue_dependencies(object, required_ids, pending_ids);
        plugin.objects.push(object.clone());
    }
}

fn import_vanilla_foundations(
    plugin: &mut Plugin,
    base_plugins: &[Plugin],
    defined_ids: &mut HashSet<String>,
    defined_effects: &mut HashSet<EffectId>,
    required_ids: &mut HashSet<String>,
    pending_ids: &mut VecDeque<String>,
) {
    for base_plugin in base_plugins {
        for object in &base_plugin.objects {
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
                    enqueue_dependencies(object, required_ids, pending_ids);
                    plugin.objects.push(object.clone());
                }
            } else {
                let id = object.editor_id().to_ascii_lowercase();
                if defined_ids.insert(id) {
                    enqueue_dependencies(object, required_ids, pending_ids);
                    plugin.objects.push(object.clone());
                }
            }
        }
    }
}

fn enqueue_dependencies(
    object: &TES3Object,
    required_ids: &mut HashSet<String>,
    pending_ids: &mut VecDeque<String>,
) {
    let mut dependencies: Vec<_> = collect_required_ids_from_object(object)
        .into_iter()
        .collect();
    dependencies.sort();
    for id in dependencies {
        if !id.is_empty() && required_ids.insert(id.clone()) {
            pending_ids.push_back(id);
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

fn collect_required_ids(plugin: &Plugin) -> HashSet<String> {
    plugin
        .objects
        .iter()
        .flat_map(collect_required_ids_from_object)
        .collect()
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
        AiEscortPackage, AiFollowPackage, Dialogue, Filter, MiscItem, Npc, Race, Spell, Static,
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

    fn dialogue_info(id: &str, previous: &str, next: &str) -> TES3Object {
        TES3Object::DialogueInfo(dialogue_info_value(id, previous, next))
    }

    fn dialogue_info_value(id: &str, previous: &str, next: &str) -> DialogueInfo {
        DialogueInfo {
            id: id.to_string(),
            prev_id: previous.to_string(),
            next_id: next.to_string(),
            ..Default::default()
        }
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
        );

        assert!(has_id(&plugin, "first_dependency"));
        assert!(has_id(&plugin, "second_dependency"));
        assert!(has_id(&plugin, "third_dependency"));
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
        let masters = vec![Plugin {
            objects: vec![
                dialogue("Topic"),
                dialogue_info("V1", "", "V2"),
                dialogue_info("V2", "V1", "V3"),
                dialogue_info("V3", "V2", ""),
            ],
        }];
        let mut plugin = Plugin {
            objects: vec![
                dialogue("Topic"),
                dialogue_info("S1", "", "V1"),
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
            vec![("S1", "", "S2"), ("S2", "S1", "S3"), ("S3", "S2", ""),]
        );
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
}
