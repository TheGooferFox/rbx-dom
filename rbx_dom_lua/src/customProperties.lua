-- Defines how to read and write properties that aren't directly scriptable.
-- The reflection database refers to these as having scriptability = "Custom"

local CollectionService = game:GetService("CollectionService")
local InsertService = game:GetService("InsertService")

return {
	Instance = {
		Attributes = {
			read = function(instance)
				return true, instance:GetAttributes()
			end,

			write = function(instance, _, value)
				local existing = instance:GetAttributes()

				for key, attr in pairs(value) do
					instance:SetAttribute(key, attr)
				end

				for key in pairs(existing) do
					if value[key] == nil then
						instance:SetAttribute(key, nil)
					end
				end

				return true
			end,
		},

		Tags = {
			read = function(instance)
				return true, CollectionService:GetTags(instance)
			end,

			write = function(instance, _, value)
				local existingTags = CollectionService:GetTags(instance)
				local unseenTags = {}

				for _, tag in ipairs(existingTags) do
					unseenTags[tag] = true
				end

				for _, tag in ipairs(value) do
					unseenTags[tag] = nil
					CollectionService:AddTag(instance, tag)
				end

				for tag in pairs(unseenTags) do
					CollectionService:RemoveTag(instance, tag)
				end

				return true
			end,
		},
	},
	LocalizationTable = {
		Contents = {
			read = function(instance, key)
				return true, instance:GetContents()
			end,

			write = function(instance, key, value)
				instance:SetContents(value)
				return true
			end,
		},
	},
	MeshPart = {
		MeshId = {
			read = function(meshPart: MeshPart)
				return true, meshPart.MeshId
			end,

			write = function(meshPart: MeshPart, _, meshId: string)
				task.spawn(function ()
					-- Use default settings to do this as immediately as possible.
					local import = InsertService:CreateMeshPartAsync(meshId, Enum.CollisionFidelity.Box, Enum.RenderFidelity.Precise)
					
					-- Cache the desired fidelity settings.
					local collisionFidelity = meshPart.CollisionFidelity
					local renderFidelity = meshPart.RenderFidelity

					-- Apply the mesh.
					meshPart:ApplyMesh(import)

					-- Let studio handle the fidelities in serial.
					meshPart.CollisionFidelity = collisionFidelity
					meshPart.RenderFidelity = renderFidelity
				end)

				return true
			end,
		},
	},
}
